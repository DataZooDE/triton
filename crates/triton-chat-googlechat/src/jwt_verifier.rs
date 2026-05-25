//! Google Chat inbound JWT verification (FR-I-8 + FR-I-3).
//!
//! Google posts events to our webhook with `Authorization: Bearer
//! <JWT>` where the JWT is signed by `chat@system.gserviceaccount.com`.
//! The canonical key source is the v1 `metadata/x509/...` endpoint,
//! which serves a JSON object whose values are PEM-wrapped X.509
//! certificates rather than the JWKS-formatted JSON our OIDC
//! verifier consumes. We accept BOTH shapes:
//!
//!   1. **PEM-cert map** (canonical): `{ "<kid>": "-----BEGIN
//!      CERTIFICATE-----\n...\n-----END CERTIFICATE-----" }` — the
//!      real Google response.
//!   2. **JWKS object** (test convenience): `{"keys":[{"kty":"RSA",
//!      "kid":"...","n":"...","e":"AQAB"}]}` — handy when the test
//!      fixture wants to produce keys with `jsonwebtoken::EncodingKey`
//!      and not deal with X.509 PEM at all. The adapter accepts
//!      this so the integration-test fixture doesn't have to bake
//!      a CA.
//!
//! Only RS256 is accepted (FR-I-3 narrowed to what Google actually
//! emits — the per-token-single-alg discipline from
//! `triton-identity` applies here too).
//!
//! The keyset is cached for at least the `cache_ttl` duration; a
//! cache miss triggers at most one refresh and concurrent missers
//! single-flight through the same lock.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

const GOOGLE_CHAT_ISSUER: &str = "chat@system.gserviceaccount.com";
/// Clock skew Google's docs implicitly allow on inbound JWTs (the
/// platform stamps `iat`/`exp` against its own clock, which can
/// drift up to a few minutes). We allow 5 minutes each direction.
const CLOCK_SKEW_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct VerifierConfig {
    pub jwks_uri: String,
    pub audience: String,
    /// How long a fetched keyset stays valid before the next
    /// refresh. Defaults to 5 minutes.
    pub cache_ttl: Duration,
}

impl VerifierConfig {
    pub fn new(jwks_uri: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            jwks_uri: jwks_uri.into(),
            audience: audience.into(),
            cache_ttl: Duration::from_secs(5 * 60),
        }
    }
}

/// Tiny claims subset we read off Google Chat JWTs. The platform
/// fills more (`name`, `email`, `email_verified`, `iat`, ...); we
/// only consume what the audit pivot needs.
#[derive(Debug, Deserialize)]
pub struct GoogleChatClaims {
    pub iss: String,
    pub aud: String,
    #[serde(default)]
    pub sub: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("missing kid")]
    MissingKid,
    #[error("unsupported alg {alg:?}; only RS256 accepted (FR-I-3)")]
    UnsupportedAlg { alg: Algorithm },
    #[error("unknown kid `{kid}`")]
    UnknownKid { kid: String },
    #[error("verify failed: {0}")]
    VerifyFailed(String),
    #[error("unexpected issuer `{0}`")]
    BadIssuer(String),
    #[error("unexpected audience")]
    BadAudience,
    #[error("jwks fetch: {0}")]
    JwksFetch(String),
    #[error("jwks parse: {0}")]
    JwksParse(String),
    #[error("decode header: {0}")]
    BadHeader(String),
}

pub struct GoogleJwtVerifier {
    config: VerifierConfig,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, DecodingKey>>,
    /// When the cache was last successfully populated. None until
    /// the first refresh succeeds.
    last_refresh: RwLock<Option<Instant>>,
    /// Single-flight: only one refresh in flight at a time so a
    /// burst of concurrent missers fans into one outbound HTTP
    /// request, not N.
    refresh_lock: Mutex<()>,
}

impl GoogleJwtVerifier {
    pub fn new(config: VerifierConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
            keys: RwLock::new(HashMap::new()),
            last_refresh: RwLock::new(None),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Verify a raw JWT. On success returns the parsed claims;
    /// on failure returns a typed error the caller maps to a
    /// rejection audit line (FR-AU-1 `phase: rejected`).
    pub async fn verify(&self, raw_token: &str) -> Result<GoogleChatClaims, VerifyError> {
        let header = decode_header(raw_token).map_err(|e| VerifyError::BadHeader(e.to_string()))?;
        if header.alg != Algorithm::RS256 {
            return Err(VerifyError::UnsupportedAlg { alg: header.alg });
        }
        let kid = header.kid.ok_or(VerifyError::MissingKid)?;
        let key = self.lookup_key(&kid).await?;

        // FR-I-3 single-alg validation form (see triton-identity
        // for why we don't widen `validation.algorithms`).
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[GOOGLE_CHAT_ISSUER]);
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.leeway = CLOCK_SKEW_SECS;

        let token = decode::<GoogleChatClaims>(raw_token, &key, &validation)
            .map_err(|e| VerifyError::VerifyFailed(e.to_string()))?;
        let claims = token.claims;
        // Belt-and-braces: jsonwebtoken's validator already enforces
        // these, but a future bump that loosens defaults would
        // silently let an off-issuer token through. Explicit checks
        // here keep the intent in code.
        if claims.iss != GOOGLE_CHAT_ISSUER {
            return Err(VerifyError::BadIssuer(claims.iss));
        }
        if claims.aud != self.config.audience {
            return Err(VerifyError::BadAudience);
        }
        Ok(claims)
    }

    async fn lookup_key(&self, kid: &str) -> Result<DecodingKey, VerifyError> {
        if !self.cache_stale().await
            && let Some(k) = self.keys.read().await.get(kid)
        {
            return Ok(k.clone());
        }
        let _guard = self.refresh_lock.lock().await;
        // Re-check after acquiring the lock — another waiter may
        // have refreshed while we queued.
        if !self.cache_stale().await
            && let Some(k) = self.keys.read().await.get(kid)
        {
            return Ok(k.clone());
        }
        self.refresh().await?;
        self.keys
            .read()
            .await
            .get(kid)
            .cloned()
            .ok_or_else(|| VerifyError::UnknownKid {
                kid: kid.to_string(),
            })
    }

    async fn cache_stale(&self) -> bool {
        match *self.last_refresh.read().await {
            Some(t) => t.elapsed() >= self.config.cache_ttl,
            None => true,
        }
    }

    async fn refresh(&self) -> Result<(), VerifyError> {
        let resp = self
            .http
            .get(&self.config.jwks_uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| VerifyError::JwksFetch(e.to_string()))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| VerifyError::JwksParse(e.to_string()))?;
        let next = parse_keyset(&body)?;
        *self.keys.write().await = next;
        *self.last_refresh.write().await = Some(Instant::now());
        Ok(())
    }
}

/// Parse either Google's PEM-cert map or a JWKS-object response.
/// Public for the unit tests; the verifier itself only reaches it
/// via `refresh`.
pub fn parse_keyset(body: &Value) -> Result<HashMap<String, DecodingKey>, VerifyError> {
    // Try JWKS shape first (objects with a `keys` array). If absent,
    // assume the canonical Google PEM-cert map.
    if let Some(keys) = body.get("keys").and_then(|v| v.as_array()) {
        let mut out = HashMap::with_capacity(keys.len());
        for k in keys {
            let Some(kid) = k.get("kid").and_then(|v| v.as_str()) else {
                continue;
            };
            // Only RSA keys: Google rotates RSA pairs for chat JWTs.
            let kty = k.get("kty").and_then(|v| v.as_str()).unwrap_or("");
            if kty != "RSA" {
                continue;
            }
            let n = k
                .get("n")
                .and_then(|v| v.as_str())
                .ok_or_else(|| VerifyError::JwksParse(format!("kid {kid} missing n")))?;
            let e = k
                .get("e")
                .and_then(|v| v.as_str())
                .ok_or_else(|| VerifyError::JwksParse(format!("kid {kid} missing e")))?;
            let key = DecodingKey::from_rsa_components(n, e)
                .map_err(|err| VerifyError::JwksParse(format!("kid {kid}: {err}")))?;
            out.insert(kid.to_string(), key);
        }
        return Ok(out);
    }
    let Some(obj) = body.as_object() else {
        return Err(VerifyError::JwksParse(
            "keyset is neither JWKS nor PEM-cert map".into(),
        ));
    };
    let mut out = HashMap::with_capacity(obj.len());
    for (kid, v) in obj {
        let Some(pem) = v.as_str() else {
            continue;
        };
        let key = DecodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|e| VerifyError::JwksParse(format!("kid {kid} PEM: {e}")))?;
        out.insert(kid.clone(), key);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_keyset_accepts_jwks_shape() {
        // n/e here are bogus — we just check the parser accepts the
        // shape; verification is exercised end-to-end in the
        // integration tests.
        let body = json!({
            "keys": [{
                "kty": "RSA",
                "kid": "test-kid",
                "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
                "e": "AQAB"
            }]
        });
        let map = parse_keyset(&body).expect("parse jwks");
        assert!(map.contains_key("test-kid"));
    }

    #[test]
    fn parse_keyset_rejects_garbage() {
        let body = json!("not an object");
        assert!(parse_keyset(&body).is_err());
    }
}
