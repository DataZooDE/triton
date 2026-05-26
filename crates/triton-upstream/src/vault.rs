//! Vault per-call OIDC swap (FR-U-2, NFR-S-3). Mints a short-lived
//! identity token via Vault's `/v1/identity/oidc/token/<role>` plugin.

use std::time::Duration;

use serde::Deserialize;
use triton_secrets::VaultToken;

#[derive(Clone)]
pub struct VaultClient {
    base: String,
    token: VaultToken,
    http: reqwest::Client,
}

impl VaultClient {
    pub fn new(base_url: impl Into<String>, token: VaultToken) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Mint a fresh OIDC token under `role`. Returns the raw token
    /// string (a JWT in production; opaque in tests). Caller is
    /// responsible for treating the token as bearer-shaped and never
    /// logging it.
    ///
    /// If Vault rejects our auth token (401/403) — e.g. the lease was
    /// revoked server-side before its proactive refresh — we
    /// invalidate the cached token and retry once, forcing a
    /// re-login. A second auth failure surfaces as an error.
    pub async fn mint_oidc(&self, role: &str) -> Result<String, String> {
        match self.mint_once(role).await {
            Ok(token) => Ok(token),
            Err(MintError::Auth(_)) => {
                self.token.invalidate().await;
                self.mint_once(role).await.map_err(MintError::into_message)
            }
            Err(other) => Err(other.into_message()),
        }
    }

    async fn mint_once(&self, role: &str) -> Result<String, MintError> {
        let url = format!("{}/v1/identity/oidc/token/{role}", self.base);
        let vault_token = self
            .token
            .get()
            .await
            .map_err(|e| MintError::Other(format!("vault auth: {e}")))?;
        let resp = self
            .http
            .get(&url)
            .header("X-Vault-Token", &vault_token)
            .send()
            .await
            .map_err(|e| MintError::Other(format!("GET {url}: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(MintError::Auth(format!(
                "GET {url}: vault rejected token ({status})"
            )));
        }
        let resp: TokenResponse = resp
            .error_for_status()
            .map_err(|e| MintError::Other(format!("GET {url}: {e}")))?
            .json()
            .await
            .map_err(|e| MintError::Other(format!("decode {url}: {e}")))?;
        if resp.data.ttl > 300 {
            // NFR-S-3 sanity: TTL ≤ 5 min. Vault role config
            // SHOULD enforce this; reject locally if the issuer
            // misconfigured.
            return Err(MintError::Other(format!(
                "vault returned a token with ttl={}s, exceeds NFR-S-3 cap of 300s",
                resp.data.ttl
            )));
        }
        Ok(resp.data.token)
    }
}

/// Internal mint outcome. `Auth` (Vault rejected our token) is the
/// only retryable case; everything else is terminal.
enum MintError {
    Auth(String),
    Other(String),
}

impl MintError {
    fn into_message(self) -> String {
        match self {
            MintError::Auth(s) | MintError::Other(s) => s,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    data: TokenData,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    token: String,
    /// Required. `#[serde(default)]` would silently let a missing
    /// `ttl` field pass the NFR-S-3 cap as `0` — fail closed instead.
    ttl: u64,
}
