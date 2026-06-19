//! Identity boundary for the HTTP trio: OIDC bearer verification
//! against the substrate issuer (FR-I-1..3).
//!
//! Per FR-I-2 the verifier holds a per-`kid` JWKS cache with
//! rate-limited refresh. Per FR-I-3 only RS256/384/512, ES256/384,
//! and EdDSA are accepted; `none` and symmetric algorithms are
//! rejected at the algorithm-allowlist stage.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use triton_core::{Principal, TritonError};

pub mod signer;
pub use signer::JwtSigner;

/// Algorithm allowlist per FR-I-3. `none` and any HS* are absent
/// by construction.
const ALLOWED_ALGS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// Configuration to build an [`OidcVerifier`].
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    /// Explicit JWKS document URL. When set, key refresh fetches this
    /// URL directly and skips OIDC discovery entirely — for issuers
    /// that publish keys without a `/.well-known/openid-configuration`
    /// endpoint (#100: an upstream agent serving its own JWKS for the
    /// outbound surface). The token `iss` claim is still validated
    /// against `issuer`, so the trust anchor stays the (issuer, JWKS)
    /// pair the operator configured.
    pub jwks_url: Option<String>,
    /// Minimum interval between JWKS refreshes for the same `kid`
    /// (FR-I-2 anti-DoS guard). Default 30 s.
    pub refresh_interval: Duration,
}

impl OidcConfig {
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            jwks_url: None,
            refresh_interval: Duration::from_secs(30),
        }
    }

    /// Pin the JWKS document URL, bypassing OIDC discovery (#100).
    pub fn with_jwks_url(mut self, jwks_url: impl Into<String>) -> Self {
        self.jwks_url = Some(jwks_url.into());
        self
    }
}

pub struct OidcVerifier {
    config: OidcConfig,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, DecodingKey>>,
    /// Per-`kid` timestamps of the last refresh attempt — FR-I-2
    /// rate-limits the JWKS fetch *per-`kid`*, not globally, so an
    /// attacker who probes a thousand unknown `kid`s doesn't lock
    /// out the legitimate-next-kid window.
    last_refresh_per_kid: RwLock<HashMap<String, Instant>>,
    /// Single-flight guard: held across the discovery + JWKS fetch
    /// so a burst of concurrent unknown-`kid` misses fans into one
    /// outbound request, not N.
    refresh_lock: Mutex<()>,
}

impl OidcVerifier {
    pub fn new(config: OidcConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
            keys: RwLock::new(HashMap::new()),
            last_refresh_per_kid: RwLock::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Verify a bearer token and build the resulting [`Principal`].
    /// Errors surface as `TritonError::Auth`; never panics.
    pub async fn verify(&self, raw_token: &str) -> Result<Principal, TritonError> {
        let header = decode_header(raw_token)
            .map_err(|e| TritonError::Auth(format!("invalid JWT header: {e}")))?;
        if !ALLOWED_ALGS.contains(&header.alg) {
            return Err(TritonError::Auth(format!(
                "alg {:?} is not in the FR-I-3 allowlist",
                header.alg
            )));
        }
        let Some(kid) = header.kid.as_ref() else {
            return Err(TritonError::Auth("JWT header missing kid".into()));
        };

        let key = self.lookup_key(kid).await?;

        // Keep `validation.algorithms = [header.alg]` (the default
        // from `Validation::new`). The up-front `ALLOWED_ALGS` check
        // above already enforces FR-I-3; **do not** widen
        // `validation.algorithms` to cover the full allowlist. In
        // jsonwebtoken 9.3 a multi-family algorithm list (e.g.
        // [RS256, EdDSA, ...]) causes `decode` to return
        // `InvalidAlgorithm` for EdDSA tokens — the per-token
        // single-alg form is the only one that works. See
        // `doc/realizations.md` §7.
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);

        let token = decode::<TokenClaims>(raw_token, &key, &validation)
            .map_err(|e| TritonError::Auth(format!("JWT verification failed: {e}")))?;
        let claims = token.claims;
        let scopes = claims.scopes();
        let groups = claims.groups();
        Ok(Principal {
            sub: claims.sub,
            scopes,
            groups,
            tenant: claims.tenant.unwrap_or_else(|| "-".to_string()),
            raw_token: raw_token.to_string(),
            trace_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    async fn lookup_key(&self, kid: &str) -> Result<DecodingKey, TritonError> {
        // Fast path: cache hit.
        if let Some(k) = self.keys.read().await.get(kid) {
            return Ok(k.clone());
        }

        // FR-I-2 per-`kid` rate limit: if we've already tried to
        // refresh for *this* kid within the window, fail fast.
        if let Some(last) = self.last_refresh_per_kid.read().await.get(kid)
            && last.elapsed() < self.config.refresh_interval
        {
            // Keep the attacker-controlled `kid` out of the
            // client-facing message (it's reflected back via the
            // adapter error body); log it at debug for diagnosis.
            tracing::debug!(kid = %kid, "JWKS refresh rate-limited for unknown kid");
            return Err(TritonError::Auth("unknown signing key".into()));
        }

        // Single-flight: only one refresh in flight across the
        // verifier. Concurrent missers serialise here and re-check
        // the cache after the leader has populated it.
        let _guard = self.refresh_lock.lock().await;
        if let Some(k) = self.keys.read().await.get(kid) {
            return Ok(k.clone());
        }
        // Re-check the rate limit; another waiter may have refreshed
        // while we were queueing for the lock.
        if let Some(last) = self.last_refresh_per_kid.read().await.get(kid)
            && last.elapsed() < self.config.refresh_interval
        {
            tracing::debug!(kid = %kid, "JWKS refresh rate-limited for unknown kid");
            return Err(TritonError::Auth("unknown signing key".into()));
        }

        self.last_refresh_per_kid
            .write()
            .await
            .insert(kid.to_string(), Instant::now());
        self.refresh_jwks().await?;
        match self.keys.read().await.get(kid).cloned() {
            Some(k) => Ok(k),
            None => {
                tracing::debug!(kid = %kid, "kid not present in refreshed JWKS");
                Err(TritonError::Auth("unknown signing key".into()))
            }
        }
    }

    async fn refresh_jwks(&self) -> Result<(), TritonError> {
        // Explicit JWKS URL (#100): fetch the document directly, no
        // discovery round-trip. The discovery-doc issuer mix-up check
        // below has no equivalent here — a raw JWKS carries no issuer
        // — but the operator pinned the (issuer, JWKS URL) pair
        // together, and `verify` still enforces `iss` on every token.
        if let Some(jwks_url) = &self.config.jwks_url {
            let jwks = self.fetch_jwks(jwks_url).await?;
            self.install_keys(&jwks).await;
            return Ok(());
        }

        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let discovery: DiscoveryDoc = self
            .http
            .get(&discovery_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| TritonError::Provider(format!("OIDC discovery {discovery_url}: {e}")))?
            .json()
            .await
            .map_err(|e| TritonError::Provider(format!("OIDC discovery decode: {e}")))?;

        // Mix-up defence: the discovery doc's `issuer` MUST match
        // the configured issuer. Otherwise a compromised DNS or
        // accidental misconfiguration could point us at a foreign
        // JWKS that signs tokens for a different identity domain.
        let doc_iss = discovery.issuer.trim_end_matches('/');
        let cfg_iss = self.config.issuer.trim_end_matches('/');
        if doc_iss != cfg_iss {
            return Err(TritonError::Provider(format!(
                "OIDC discovery issuer {doc_iss} != configured {cfg_iss}"
            )));
        }

        let jwks = self.fetch_jwks(&discovery.jwks_uri).await?;
        self.install_keys(&jwks).await;
        Ok(())
    }

    async fn fetch_jwks(&self, jwks_url: &str) -> Result<JwkSet, TritonError> {
        self.http
            .get(jwks_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| TritonError::Provider(format!("JWKS fetch {jwks_url}: {e}")))?
            .json()
            .await
            .map_err(|e| TritonError::Provider(format!("JWKS decode: {e}")))
    }

    async fn install_keys(&self, jwks: &JwkSet) {
        let mut next = HashMap::new();
        for jwk in &jwks.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            // `DecodingKey::from_jwk` does the right thing for every
            // JWK shape (RSA / EC / OKP); rolling our own pattern
            // match earlier dropped the family metadata on the
            // resulting key and produced InvalidAlgorithm on verify.
            let key = match DecodingKey::from_jwk(jwk) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(kid, ?e, "skipping JWK we cannot decode");
                    continue;
                }
            };
            next.insert(kid, key);
        }
        *self.keys.write().await = next;
    }
}

#[derive(Debug, Deserialize)]
struct DiscoveryDoc {
    issuer: String,
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    sub: String,
    #[serde(default)]
    tenant: Option<String>,
    /// OAuth2 RFC 6749 single-string form; whitespace-split into
    /// scopes. Some issuers use the `scp` array form instead.
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scp: Option<Vec<String>>,
    /// Group/role memberships. Read from `roles` (the common OIDC/Keycloak
    /// convention, and escurel's default groups claim), falling back to
    /// `groups`. Carried on the [`Principal`] for opt-in forwarding.
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    groups: Option<Vec<String>>,
}

impl TokenClaims {
    fn scopes(&self) -> Vec<String> {
        if let Some(s) = &self.scp {
            return s.clone();
        }
        if let Some(s) = &self.scope {
            return s.split_whitespace().map(str::to_string).collect();
        }
        Vec::new()
    }

    fn groups(&self) -> Vec<String> {
        if let Some(g) = &self.roles {
            return g.clone();
        }
        if let Some(g) = &self.groups {
            return g.clone();
        }
        Vec::new()
    }
}
