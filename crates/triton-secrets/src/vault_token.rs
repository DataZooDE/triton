//! Vault token source for Triton's live Vault calls (KV secret reads
//! at boot + per-request OIDC minting in the upstream router).
//!
//! Two modes:
//!
//! * [`VaultToken::fixed`] — a static token (`TRITON_VAULT_TOKEN`).
//!   The substrate discourages this (`substrate-platform` ref 02:
//!   "❌ static tokens"); kept for local dev / a hand-issued token.
//! * [`VaultToken::workload_identity`] — the substrate-blessed path.
//!   Triton authenticates to Vault itself using the Nomad-issued
//!   workload-identity JWT (an `identity { aud = ["vault"] }` stanza
//!   writes it to a file), logging in at `auth/<mount>/login` and
//!   caching the returned client token until shortly before its lease
//!   expires. No long-lived token is ever handed to the process.
//!
//! The handle is cheap to [`Clone`] (it's `Arc`-backed) so the secret
//! resolver and the upstream router share one token + one login.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

/// A shared, refreshing source of a Vault token.
#[derive(Clone)]
pub struct VaultToken {
    inner: Arc<Inner>,
}

enum Inner {
    Fixed(String),
    Wi(Wi),
}

struct Wi {
    base: String,
    jwt_path: PathBuf,
    mount: String,
    role: String,
    http: reqwest::Client,
    cache: Mutex<Option<Cached>>,
}

struct Cached {
    token: String,
    /// When to proactively re-login (half the lease, floored).
    refresh_at: Instant,
}

impl VaultToken {
    /// A static token (kept verbatim; never logged).
    pub fn fixed(token: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner::Fixed(token.into())),
        }
    }

    /// Authenticate via Nomad workload identity: read the JWT at
    /// `jwt_path` and exchange it at `<base>/v1/auth/<mount>/login`
    /// for a Vault token, refreshing before the lease expires.
    pub fn workload_identity(
        base_url: impl Into<String>,
        jwt_path: impl Into<PathBuf>,
        auth_mount: impl Into<String>,
        role: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            inner: Arc::new(Inner::Wi(Wi {
                base: base_url.into().trim_end_matches('/').to_string(),
                jwt_path: jwt_path.into(),
                mount: auth_mount.into().trim_matches('/').to_string(),
                role: role.into(),
                http,
                cache: Mutex::new(None),
            })),
        }
    }

    /// The current Vault token, logging in / refreshing as needed.
    pub async fn get(&self) -> Result<String, VaultAuthError> {
        match &*self.inner {
            Inner::Fixed(t) => Ok(t.clone()),
            Inner::Wi(wi) => {
                let mut cache = wi.cache.lock().await;
                if let Some(c) = cache.as_ref()
                    && Instant::now() < c.refresh_at
                {
                    return Ok(c.token.clone());
                }
                let fresh = wi.login().await?;
                let token = fresh.token.clone();
                *cache = Some(fresh);
                Ok(token)
            }
        }
    }
}

impl Wi {
    async fn login(&self) -> Result<Cached, VaultAuthError> {
        // Re-read the JWT every login — Nomad rotates the file, so a
        // cached JWT would eventually be stale.
        let jwt = std::fs::read_to_string(&self.jwt_path)
            .map_err(|e| VaultAuthError::JwtRead {
                path: self.jwt_path.display().to_string(),
                detail: e.to_string(),
            })?
            .trim()
            .to_string();
        if jwt.is_empty() {
            return Err(VaultAuthError::JwtRead {
                path: self.jwt_path.display().to_string(),
                detail: "jwt file is empty".into(),
            });
        }
        let url = format!("{}/v1/auth/{}/login", self.base, self.mount);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "role": self.role, "jwt": jwt }))
            .send()
            .await
            .map_err(|e| VaultAuthError::Transport {
                url: url.clone(),
                detail: e.to_string(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(VaultAuthError::Status {
                url: url.clone(),
                status: status.as_u16(),
            });
        }
        let body: LoginResponse = resp.json().await.map_err(|e| VaultAuthError::Decode {
            url: url.clone(),
            detail: e.to_string(),
        })?;
        let auth = body.auth.ok_or(VaultAuthError::MissingAuth { url })?;
        if auth.client_token.is_empty() {
            return Err(VaultAuthError::EmptyToken);
        }
        Ok(Cached {
            token: auth.client_token,
            refresh_at: Instant::now() + refresh_after(auth.lease_duration),
        })
    }
}

/// Renew at half the lease; treat a 0 (root/∞) lease as a 1h re-check
/// so a misconfigured infinite token still rotates. A 10s floor avoids
/// hammering Vault on small leases — but the floor must never push the
/// refresh to or past expiry, so for very short leases we fall back to
/// the (always-before-expiry) half-lease value.
fn refresh_after(lease_secs: u64) -> Duration {
    if lease_secs == 0 {
        return Duration::from_secs(3600);
    }
    let half = lease_secs / 2;
    let floored = half.max(10);
    let secs = if floored >= lease_secs {
        half.max(1)
    } else {
        floored
    };
    Duration::from_secs(secs)
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    auth: Option<AuthData>,
}

#[derive(Debug, Deserialize)]
struct AuthData {
    client_token: String,
    #[serde(default)]
    lease_duration: u64,
}

/// Errors obtaining a Vault token. Surfaced into the resolver's
/// boot-time errors and the router's per-call errors; the JWT and
/// token never appear in the message.
#[derive(Debug, thiserror::Error)]
pub enum VaultAuthError {
    #[error("could not read workload-identity JWT at {path}: {detail}")]
    JwtRead { path: String, detail: String },
    #[error("vault login transport error on {url}: {detail}")]
    Transport { url: String, detail: String },
    #[error("vault login non-2xx on {url}: {status}")]
    Status { url: String, status: u16 },
    #[error("vault login decode failed on {url}: {detail}")]
    Decode { url: String, detail: String },
    #[error("vault login response on {url} had no `auth` block")]
    MissingAuth { url: String },
    #[error("vault login returned an empty client_token")]
    EmptyToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_after_halves_the_lease_with_a_floor() {
        assert_eq!(refresh_after(3600), Duration::from_secs(1800));
        assert_eq!(refresh_after(60), Duration::from_secs(30));
        assert_eq!(refresh_after(0), Duration::from_secs(3600)); // ∞ → recheck
    }

    #[test]
    fn refresh_after_never_lands_at_or_past_expiry_for_short_leases() {
        // The 10s floor must not push the refresh to/past the lease.
        // (A 1s lease is degenerate — not expressible sub-second.)
        for lease in 2..=30u64 {
            let r = refresh_after(lease).as_secs();
            assert!(
                r < lease,
                "lease={lease}s would refresh at {r}s — not before expiry"
            );
        }
    }

    #[tokio::test]
    async fn fixed_token_returns_verbatim() {
        let t = VaultToken::fixed("static-abc");
        assert_eq!(t.get().await.unwrap(), "static-abc");
        // Clones share the same source.
        assert_eq!(t.clone().get().await.unwrap(), "static-abc");
    }
}
