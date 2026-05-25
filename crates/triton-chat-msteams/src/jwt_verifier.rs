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
    /// PR 37: NFR-S-4 host allowlist for the inbound JWT's
    /// `serviceUrl` claim. Production builds use the documented
    /// Microsoft hosts only ([`SERVICE_URL_HOST_SUFFIXES`]); test
    /// fixtures pass additional `127.0.0.1` / fake-host entries via
    /// `with_extra_service_url_hosts`. A nontrivial value is fatal
    /// outside `local` env (the binary enforces that at wiring time).
    extra_service_url_hosts: Vec<String>,
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

/// NFR-S-4 host allowlist for the Bot Framework `serviceUrl`
/// reply target. Even a correctly-signed JWT could carry an
/// arbitrary `serviceUrl` (e.g. one minted by a Microsoft
/// developer playground); the adapter must refuse to POST reply
/// activities to anything outside Microsoft's documented service-
/// URL shapes. Suffixes are matched on a DNS-label boundary —
/// `*.botframework.com.evil.example` does NOT pass.
///
/// Documented hosts (Bot Framework / Teams):
///   * `*.botframework.com` (channel-direct service URLs)
///   * `*.trafficmanager.net` (the Teams channel's documented
///     reply target, e.g. `https://smba.trafficmanager.net/teams/`)
pub const SERVICE_URL_HOST_SUFFIXES: &[&str] = &[".botframework.com", ".trafficmanager.net"];

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
    #[error("jwt `serviceUrl` `{0}` is not on the bot framework host allowlist")]
    UntrustedServiceUrl(String),
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
            extra_service_url_hosts: Vec::new(),
        }
    }

    /// Extend the `serviceUrl` host allowlist with additional hosts.
    /// Only meaningful for the integration test fixture (the fake bot
    /// framework binds at `127.0.0.1:<port>`, which isn't on the
    /// production list). The binary refuses to populate this outside
    /// `local` env, so a misconfigured production deploy can never
    /// reach this entry point.
    pub fn with_extra_service_url_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_service_url_hosts
            .extend(hosts.into_iter().map(Into::into));
        self
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
        // "InvalidIssuer".
        validation.validate_aud = true;
        validation.validate_exp = true;
        // PR 37 Finding 4 (HIGH): jsonwebtoken's default exp/nbf
        // leeway is 60s, NOT the 5min skew the comment block above
        // claimed. Explicitly set 300s (5 min) — Microsoft's Bot
        // Framework SDK does the same — so a JWT minted seconds
        // before a brief clock drift still validates.
        validation.leeway = 300;

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
        // PR 37: NFR-S-4 host allowlist. A correctly-signed JWT can
        // still come from a Bot Framework developer playground that
        // sets `serviceUrl` to an attacker-controlled host. Refuse
        // anything outside Microsoft's documented shapes (plus the
        // test fixture's extra hosts, when configured) so the
        // outbound reply Activity never POSTs to a non-Microsoft
        // endpoint.
        if !service_url_host_allowed_with_extras(
            &data.claims.service_url,
            &self.extra_service_url_hosts,
        ) {
            return Err(VerifyError::UntrustedServiceUrl(data.claims.service_url));
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

/// True iff `service_url` parses as an `https` URL whose host ends
/// with one of [`SERVICE_URL_HOST_SUFFIXES`] on a DNS-label boundary.
/// Returns `false` on any parse failure or scheme mismatch — a
/// malformed claim fails closed.
///
/// Public so the integration test (and any future caller) can
/// validate ad-hoc claims without minting a full JWT.
pub fn service_url_host_allowed(service_url: &str) -> bool {
    service_url_host_allowed_with_extras(service_url, &[] as &[String])
}

/// Same as [`service_url_host_allowed`] but also accepts hosts listed
/// in `extras`. Used by the verifier when the test fixture wires in
/// `127.0.0.1` etc. via [`JwtVerifier::with_extra_service_url_hosts`].
pub fn service_url_host_allowed_with_extras<S: AsRef<str>>(
    service_url: &str,
    extras: &[S],
) -> bool {
    let Ok(parsed) = url::Url::parse(service_url) else {
        return false;
    };
    // Allow `http` ONLY when the host matches an extras entry —
    // i.e. a test fixture pointed at `http://127.0.0.1:<port>/`.
    // Production hosts MUST be `https`.
    let scheme = parsed.scheme();
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let extras_match = extras.iter().any(|e| e.as_ref() == host);
    if extras_match {
        return scheme == "http" || scheme == "https";
    }
    if scheme != "https" {
        return false;
    }
    SERVICE_URL_HOST_SUFFIXES.iter().any(|suffix| {
        let s: &str = suffix;
        if let Some(apex) = s.strip_prefix('.') {
            host == apex || host.ends_with(s)
        } else {
            host == s
        }
    })
}

#[cfg(test)]
mod tests {
    use super::service_url_host_allowed;

    // PR 37: NFR-S-4 fix. A correctly-signed JWT could still carry
    // a `serviceUrl` pointed at an attacker host (Bot Framework dev
    // playground); the adapter must refuse anything off Microsoft's
    // documented host shapes.

    #[test]
    fn allows_documented_microsoft_service_urls() {
        // Teams channel canonical (trafficmanager).
        assert!(service_url_host_allowed(
            "https://smba.trafficmanager.net/teams/"
        ));
        // Bot Framework direct (botframework.com).
        assert!(service_url_host_allowed(
            "https://smba.example.botframework.com/"
        ));
        // No-path variant.
        assert!(service_url_host_allowed("https://smba.trafficmanager.net"));
    }

    #[test]
    fn rejects_arbitrary_hosts_even_when_jwt_is_otherwise_valid() {
        assert!(!service_url_host_allowed("https://attacker.example/"));
        // Subdomain-suffix attack: ends with the magic string but
        // not on a label boundary.
        assert!(!service_url_host_allowed(
            "https://smba.trafficmanager.net.evil.example/"
        ));
        assert!(!service_url_host_allowed(
            "https://botframework.com.evil.example/"
        ));
        // Wrong scheme.
        assert!(!service_url_host_allowed("http://smba.trafficmanager.net/"));
        // Unparseable / empty.
        assert!(!service_url_host_allowed("not a url"));
        assert!(!service_url_host_allowed(""));
        // Userinfo smuggling.
        assert!(!service_url_host_allowed(
            "https://smba.trafficmanager.net@evil.example/"
        ));
    }
}
