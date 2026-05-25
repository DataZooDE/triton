//! Bot Framework JWT verifier with cached JWKS.
//!
//! Microsoft's Bot Framework signs every inbound webhook payload
//! with a key whose JWKS URI is announced under
//! `https://login.botframework.com/v1/.well-known/openidconfiguration`.
//! The connector publishes a discovery document whose `jwks_uri`
//! points at the key set; keys rotate, so we cache for a bounded
//! window (5 minutes) and refresh on cache miss / expiry.
//!
//! Verification rules (FR-I-8):
//!
//! * `iss == "https://api.botframework.com"` — note the discovery
//!   document lives under `login.botframework.com` but the issued
//!   tokens carry `api.botframework.com` as `iss`; that's how the
//!   connector identifies itself.
//! * `aud == <bot Microsoft App ID>` from the manifest.
//! * `exp` not expired (5-minute skew allowed by jsonwebtoken's
//!   default leeway).
//! * RS256 signature against a key matched by `kid` from JWKS.
//!
//! Constant-time signature comparison comes for free from
//! `jsonwebtoken` (built on `ring`, which is constant-time).

use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Default OpenID discovery endpoint for Microsoft's Bot Framework
/// channel (Teams). Production builds refuse overrides outside the
/// `local` env to keep NFR-S-4's egress allowlist enforceable.
pub const DEFAULT_OPENID_URL: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";

/// Expected `iss` value carried on Bot-Framework-signed JWTs. Note
/// this differs from the discovery URL — Microsoft's connector
/// emits its tokens under `api.botframework.com`.
const EXPECTED_ISSUER: &str = "https://api.botframework.com";

/// How long a fetched JWKS is reused before we re-discover keys.
/// 5 minutes matches the Bot Framework SDK's documented cache TTL
/// and bounds the worst-case rotation lag to roughly that window.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// HTTP timeout for OpenID discovery + JWKS fetches. We bail out
/// fast — at request time the verifier surfaces the failure as an
/// `Auth` error and the adapter records a rejection audit; we don't
/// want the inbound webhook handler to block on a slow Microsoft
/// endpoint.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Verified principal-shaped claims a Bot Framework JWT carries.
/// `service_url` is the platform-asserted base for the outbound
/// reply Activity (FR-S-4-derived; we trust it because it rode
/// inside a JWT we just verified).
#[derive(Debug, Clone)]
pub struct VerifiedClaims {
    pub service_url: String,
}

/// Bot Framework JWT verifier. One instance per adapter; the JWKS
/// cache lives on the verifier itself so a hot path skips
/// re-discovery on every request.
pub struct JwtVerifier {
    openid_url: String,
    audience: String,
    http: reqwest::Client,
    cache: Mutex<Option<CachedJwks>>,
}

struct CachedJwks {
    jwks: Arc<JwkSet>,
    fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct OpenIdDiscovery {
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct BotFrameworkClaims {
    iss: String,
    #[serde(default)]
    #[serde(rename = "serviceUrl")]
    service_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("openid discovery fetch failed: {0}")]
    Discovery(String),
    #[error("jwks fetch failed: {0}")]
    Jwks(String),
    #[error("jwt header decode failed: {0}")]
    Header(String),
    #[error("no JWKS key matched kid `{0}`")]
    UnknownKid(String),
    #[error("jwt decode failed: {0}")]
    Decode(String),
    #[error("jwt issuer does not match expected `{expected}`; got `{actual}`")]
    BadIssuer {
        actual: String,
        expected: &'static str,
    },
    #[error("jwt missing required claim `{0}`")]
    MissingClaim(&'static str),
}

impl JwtVerifier {
    pub fn new(openid_url: impl Into<String>, audience: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("reqwest client builds with valid options");
        Self {
            openid_url: openid_url.into(),
            audience: audience.into(),
            http,
            cache: Mutex::new(None),
        }
    }

    /// Verify `token`. Returns the trusted-by-derivation claims the
    /// adapter needs for the outbound path. `Err` means the request
    /// MUST be rejected with 401 and a `record_rejection` audit
    /// line.
    pub async fn verify(&self, token: &str) -> Result<VerifiedClaims, VerifyError> {
        let header = decode_header(token).map_err(|e| VerifyError::Header(e.to_string()))?;
        let kid = header.kid.ok_or(VerifyError::Header(
            "missing `kid` header — Bot Framework JWTs MUST carry one".into(),
        ))?;
        let jwks = self.jwks().await?;
        let jwk = jwks
            .find(&kid)
            .ok_or_else(|| VerifyError::UnknownKid(kid.clone()))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| VerifyError::Jwks(e.to_string()))?;

        // RS256 is what Microsoft signs Bot Framework tokens with.
        // We accept that algorithm specifically rather than the
        // jsonwebtoken default of "whatever the header says" — that
        // would let an attacker downgrade to HS256 with the public
        // key as the symmetric secret (classic JWT alg-confusion).
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[self.audience.as_str()]);
        // We check `iss` ourselves below so we can fail with a
        // typed BadIssuer error; `jsonwebtoken` would only say
        // "InvalidIssuer". jsonwebtoken still enforces `exp` with
        // its built-in 60s leeway.
        validation.validate_aud = true;
        validation.validate_exp = true;

        let data = decode::<BotFrameworkClaims>(token, &key, &validation)
            .map_err(|e| VerifyError::Decode(e.to_string()))?;
        if data.claims.iss != EXPECTED_ISSUER {
            return Err(VerifyError::BadIssuer {
                actual: data.claims.iss,
                expected: EXPECTED_ISSUER,
            });
        }
        if data.claims.service_url.is_empty() {
            return Err(VerifyError::MissingClaim("serviceUrl"));
        }
        Ok(VerifiedClaims {
            service_url: data.claims.service_url,
        })
    }

    /// Return a JWKS, fetching + caching on miss / expiry. Concurrent
    /// callers serialise behind the mutex; the fetch itself runs
    /// while holding the lock so a thundering herd at expiry only
    /// hits Microsoft once.
    async fn jwks(&self) -> Result<Arc<JwkSet>, VerifyError> {
        let mut guard = self.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && c.fetched_at.elapsed() < JWKS_CACHE_TTL
        {
            return Ok(c.jwks.clone());
        }
        let discovery: OpenIdDiscovery = self
            .http
            .get(&self.openid_url)
            .send()
            .await
            .map_err(|e| VerifyError::Discovery(e.to_string()))?
            .json()
            .await
            .map_err(|e| VerifyError::Discovery(e.to_string()))?;
        let jwks: JwkSet = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(|e| VerifyError::Jwks(e.to_string()))?
            .json()
            .await
            .map_err(|e| VerifyError::Jwks(e.to_string()))?;
        let arc = Arc::new(jwks);
        *guard = Some(CachedJwks {
            jwks: arc.clone(),
            fetched_at: Instant::now(),
        });
        Ok(arc)
    }
}
