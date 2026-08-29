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

/// Expected `iss` on Bot-Framework-signed JWTs (MULTI-tenant bots).
/// Note this differs from the discovery URL — Microsoft's connector
/// emits its tokens under `api.botframework.com`.
const EXPECTED_ISSUER: &str = "https://api.botframework.com";

/// Hardcoded Entra host used to derive a single-tenant bot's trust
/// anchor. Only a validated tenant id is ever interpolated into it
/// (NFR-S-4: the HOST is never configurable).
const ENTRA_HOST: &str = "https://login.microsoftonline.com";

/// Legacy (v1) Entra issuer host. A single-tenant bot's inbound token
/// may carry either this or the v2 form depending on which endpoint
/// minted it, so both are accepted for the SAME tenant.
const ENTRA_V1_ISSUER_HOST: &str = "https://sts.windows.net";

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
    /// The signed reply target, when the token carried one. `None` for a
    /// single-tenant bot, whose Entra token has no such claim — the
    /// caller must then take it from the Activity body and put it through
    /// [`JwtVerifier::service_url_allowed`].
    pub service_url: Option<String>,
}

impl VerifiedClaims {
    /// The reply target, after the handler has resolved it.
    ///
    /// Empty is unreachable in practice: `handle_webhook` refuses the
    /// request when neither the token nor the body yields an allowlisted
    /// serviceUrl, so anything that gets here is either signed or
    /// body-supplied AND allowlisted.
    pub fn reply_base(&self) -> &str {
        self.service_url.as_deref().unwrap_or_default()
    }
}

/// Bot Framework JWT verifier. One instance per adapter; the JWKS
/// cache lives on the verifier itself so a hot path skips
/// re-discovery on every request.
/// One place inbound tokens may legitimately come from: a discovery
/// document to fetch signing keys from, and the `iss` values a token
/// signed by those keys is allowed to carry.
///
/// There are two in practice, and a deployment may need BOTH at once —
/// an adapter serving a multi-tenant bot and a single-tenant bot has no
/// way to know which kind a given request belongs to until it looks:
///
///   * **Multi-tenant** — keys from `login.botframework.com`, `iss` is
///     `https://api.botframework.com`.
///   * **Single-tenant** — Bot Framework does NOT sign these. Entra
///     does, with the bot's home-tenant keys, so both the keyset and
///     the issuer are tenant-scoped. A verifier built only for the
///     multi-tenant shape rejects every single-tenant request, which is
///     a silent 401 that looks exactly like a misconfigured bot.
struct TrustAnchor {
    /// Whether tokens from this anchor carry a `serviceUrl` claim.
    ///
    /// Bot Framework's do — signed, and therefore trustworthy as a reply
    /// target. Entra's (single-tenant) do NOT: they are ordinary AAD
    /// access tokens, and `serviceUrl` is a Bot-Framework-specific claim.
    /// For those the reply target arrives in the Activity BODY instead,
    /// unsigned, so the host allowlist becomes the only thing standing
    /// between us and POSTing a reply wherever a caller asks.
    carries_service_url: bool,
    openid_url: String,
    /// Accepted `iss` values for tokens signed by this anchor's keys.
    issuers: Vec<String>,
    /// Per-anchor, because the two anchors publish different keysets
    /// and rotate independently.
    cache: Mutex<Option<CachedJwks>>,
}

pub struct JwtVerifier {
    anchors: Vec<TrustAnchor>,
    audience: String,
    http: reqwest::Client,
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
    BadIssuer { actual: String, expected: String },
    #[error("invalid tenant id: {0}")]
    InvalidTenant(String),
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
            anchors: vec![TrustAnchor {
                carries_service_url: true,
                openid_url: openid_url.into(),
                issuers: vec![EXPECTED_ISSUER.to_string()],
                cache: Mutex::new(None),
            }],
            audience: audience.into(),
            http,
            extra_service_url_hosts: Vec::new(),
        }
    }

    /// Additionally accept tokens for a SINGLE-TENANT bot registration
    /// (`MsaAppType: SingleTenant`), which Entra signs with the bot's
    /// home-tenant keys rather than Bot Framework's.
    ///
    /// Additive on purpose: the multi-tenant anchor stays configured, so
    /// one adapter can serve both kinds and neither is weakened. A token
    /// is still bound to exactly one anchor — the one whose keyset
    /// actually signed it — and must carry that anchor's issuer.
    ///
    /// Both Entra issuer forms are accepted for this tenant: v2
    /// (`login.microsoftonline.com/{tid}/v2.0`) and the v1 form
    /// (`sts.windows.net/{tid}/`), because which one appears depends on
    /// the endpoint that minted the token, not on anything we control.
    ///
    /// NFR-S-4 holds: the host is hardcoded and only a tenant id is
    /// interpolated, after validating it as an opaque
    /// `[A-Za-z0-9.-]` segment.
    pub fn with_single_tenant(mut self, tenant_id: &str) -> Result<Self, VerifyError> {
        let tenant = validate_tenant_id(tenant_id)?;
        self.anchors.push(TrustAnchor {
            // Entra tokens have no serviceUrl claim — see TrustAnchor.
            carries_service_url: false,
            openid_url: format!("{ENTRA_HOST}/{tenant}/v2.0/.well-known/openid-configuration"),
            issuers: vec![
                format!("{ENTRA_HOST}/{tenant}/v2.0"),
                format!("{ENTRA_V1_ISSUER_HOST}/{tenant}/"),
            ],
            cache: Mutex::new(None),
        });
        Ok(self)
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
        // Pick the anchor by KID, not by the token's own `iss`. The
        // issuer claim is attacker-controlled until the signature is
        // checked, so routing on it would mean choosing a trust anchor
        // from untrusted input; a kid is matched against keysets we
        // fetched ourselves. The first anchor publishing this kid is the
        // one that signed it — and the token must then also carry an
        // issuer THAT anchor accepts, checked below.
        let mut anchor_and_key = None;
        let mut last_err = None;
        for anchor in &self.anchors {
            match self.jwks_for(anchor).await {
                Ok(jwks) => {
                    if let Some(jwk) = jwks.find(&kid) {
                        let key = DecodingKey::from_jwk(jwk)
                            .map_err(|e| VerifyError::Jwks(e.to_string()))?;
                        anchor_and_key = Some((anchor, key));
                        break;
                    }
                }
                // A single unreachable anchor must not veto the other:
                // remember the failure and only surface it if NO anchor
                // ends up matching, so an Entra outage cannot take down
                // a multi-tenant bot (or vice versa).
                Err(e) => last_err = Some(e),
            }
        }
        let (anchor, key) = match anchor_and_key {
            Some(v) => v,
            None => return Err(last_err.unwrap_or(VerifyError::UnknownKid(kid.clone()))),
        };

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
        if !anchor.issuers.iter().any(|i| i == &data.claims.iss) {
            return Err(VerifyError::BadIssuer {
                actual: data.claims.iss,
                expected: anchor.issuers.join(" or "),
            });
        }
        if anchor.carries_service_url && data.claims.service_url.is_empty() {
            return Err(VerifyError::MissingClaim("serviceUrl"));
        }
        // PR 37: NFR-S-4 host allowlist. A correctly-signed JWT can
        // still come from a Bot Framework developer playground that
        // sets `serviceUrl` to an attacker-controlled host. Refuse
        // anything outside Microsoft's documented shapes (plus the
        // test fixture's extra hosts, when configured) so the
        // outbound reply Activity never POSTs to a non-Microsoft
        // endpoint.
        if data.claims.service_url.is_empty() {
            // Single-tenant: nothing signed to check here. The caller
            // validates the body's serviceUrl instead — it must, and
            // `service_url_allowed` is the same check.
            return Ok(VerifiedClaims { service_url: None });
        }
        if !service_url_host_allowed_with_extras(
            &data.claims.service_url,
            &self.extra_service_url_hosts,
        ) {
            return Err(VerifyError::UntrustedServiceUrl(data.claims.service_url));
        }
        Ok(VerifiedClaims {
            service_url: Some(data.claims.service_url),
        })
    }

    /// Is `url` an acceptable reply target?
    ///
    /// For a single-tenant bot the `serviceUrl` comes from the request
    /// BODY, which is not signed — so this allowlist is the whole of the
    /// protection against being told to POST a reply to an attacker's
    /// host. Same rule the signed path applies, exposed so the adapter
    /// cannot accidentally apply a weaker one.
    pub fn service_url_allowed(&self, url: &str) -> bool {
        service_url_host_allowed_with_extras(url, &self.extra_service_url_hosts)
    }

    /// Return a JWKS, fetching + caching on miss / expiry. Concurrent
    /// callers serialise behind the mutex; the fetch itself runs
    /// while holding the lock so a thundering herd at expiry only
    /// hits Microsoft once.
    async fn jwks_for(&self, anchor: &TrustAnchor) -> Result<Arc<JwkSet>, VerifyError> {
        let mut guard = anchor.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && c.fetched_at.elapsed() < JWKS_CACHE_TTL
        {
            return Ok(c.jwks.clone());
        }
        let discovery: OpenIdDiscovery = self
            .http
            .get(&anchor.openid_url)
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

/// A tenant id is interpolated into a discovery URL, so it is checked
/// as an opaque segment before use: a GUID or a verified domain, and
/// nothing that could escape the path or swap the host. Same rule, and
/// the same reasoning, as `token_client`'s.
fn validate_tenant_id(tenant_id: &str) -> Result<&str, VerifyError> {
    let t = tenant_id.trim();
    if t.is_empty()
        || !t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(VerifyError::InvalidTenant(tenant_id.to_string()));
    }
    Ok(t)
}

#[cfg(test)]
mod single_tenant_tests {
    use super::*;

    const TENANT: &str = "28c0071d-815c-4ace-a3b5-9a28bde005fd";

    #[test]
    fn multi_tenant_only_by_default() {
        // The pre-existing shape must be untouched: one anchor, Bot
        // Framework's, accepting exactly the connector's issuer.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id");
        assert_eq!(v.anchors.len(), 1);
        assert_eq!(v.anchors[0].issuers, vec![EXPECTED_ISSUER.to_string()]);
        assert_eq!(v.anchors[0].openid_url, DEFAULT_OPENID_URL);
    }

    #[test]
    fn single_tenant_is_additive_not_a_replacement() {
        // Both must be live at once: an adapter may serve a
        // multi-tenant AND a single-tenant bot, and enabling one must
        // not silently stop accepting the other.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id")
            .with_single_tenant(TENANT)
            .expect("valid tenant");
        assert_eq!(v.anchors.len(), 2);
        assert_eq!(v.anchors[0].issuers, vec![EXPECTED_ISSUER.to_string()]);

        // Entra signs single-tenant tokens, so the keyset is tenant-
        // scoped rather than Bot Framework's.
        assert_eq!(
            v.anchors[1].openid_url,
            format!(
                "https://login.microsoftonline.com/{TENANT}/v2.0/.well-known/openid-configuration"
            )
        );
        // BOTH issuer forms, because which one appears depends on the
        // endpoint that minted the token, not on anything we control.
        assert!(
            v.anchors[1]
                .issuers
                .contains(&format!("https://login.microsoftonline.com/{TENANT}/v2.0"))
        );
        assert!(
            v.anchors[1]
                .issuers
                .contains(&format!("https://sts.windows.net/{TENANT}/"))
        );
    }

    #[test]
    fn the_anchors_do_not_share_an_issuer_list() {
        // The bug worth engineering against: if the tenant anchor's
        // issuers leaked into the Bot Framework anchor (or vice versa),
        // a token signed by ONE keyset could claim the OTHER's issuer
        // and pass. Each anchor must accept only its own.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id")
            .with_single_tenant(TENANT)
            .expect("valid tenant");
        assert!(!v.anchors[0].issuers.iter().any(|i| i.contains(TENANT)));
        assert!(!v.anchors[1].issuers.iter().any(|i| i == EXPECTED_ISSUER));
    }

    #[test]
    fn a_hostile_tenant_id_is_refused_before_it_reaches_a_url() {
        // NFR-S-4: the tenant is interpolated into a discovery URL, so
        // none of these may build a verifier — each would move key
        // discovery to a host we do not control.
        for bad in [
            "",
            "   ",
            "../../evil",
            "tenant/../../x",
            "evil.com/path",
            "user@evil.com",
            "tenant?x=1",
            "https://evil.com",
            "tenant id",
        ] {
            let r = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id").with_single_tenant(bad);
            assert!(
                matches!(r, Err(VerifyError::InvalidTenant(_))),
                "tenant id {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_valid_tenant_is_trimmed_not_rejected() {
        // Trailing whitespace from a values file is an operator typo,
        // not an attack.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id")
            .with_single_tenant(&format!("  {TENANT}  "))
            .expect("whitespace is trimmed");
        assert!(v.anchors[1].openid_url.contains(TENANT));
        assert!(!v.anchors[1].openid_url.contains(' '));
    }
}

#[cfg(test)]
mod service_url_source_tests {
    use super::*;

    const TENANT: &str = "28c0071d-815c-4ace-a3b5-9a28bde005fd";

    #[test]
    fn only_the_bot_framework_anchor_expects_a_signed_service_url() {
        // This is the bug that made Teams silently 401 on 2026-08-29:
        // the verifier demanded a `serviceUrl` claim that a single-tenant
        // Entra token never carries, so signature and issuer passed and
        // the request died on a missing claim.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id")
            .with_single_tenant(TENANT)
            .expect("valid tenant");
        assert!(v.anchors[0].carries_service_url, "Bot Framework signs it");
        assert!(
            !v.anchors[1].carries_service_url,
            "Entra does not — it is a Bot-Framework-specific claim"
        );
    }

    #[test]
    fn the_body_supplied_reply_target_is_held_to_the_same_allowlist() {
        // A single-tenant serviceUrl arrives UNSIGNED in the request
        // body, so this allowlist is the only thing stopping a caller
        // from aiming our reply — which carries a real Bot Connector
        // token — at a host they control.
        let v = JwtVerifier::new(DEFAULT_OPENID_URL, "app-id");
        assert!(v.service_url_allowed("https://smba.trafficmanager.net/emea/"));
        assert!(v.service_url_allowed("https://europe.botframework.com/"));

        for hostile in [
            "https://evil.example/",
            "http://smba.trafficmanager.net/emea/", // not https
            "https://smba.trafficmanager.net.evil.example/", // suffix trick
            "https://botframework.com.attacker.test/",
            "",
        ] {
            assert!(
                !v.service_url_allowed(hostile),
                "must refuse reply target {hostile:?}"
            );
        }
    }

    #[test]
    fn reply_base_is_empty_only_when_unresolved() {
        assert_eq!(VerifiedClaims { service_url: None }.reply_base(), "");
        assert_eq!(
            VerifiedClaims {
                service_url: Some("https://smba.trafficmanager.net/emea/".into())
            }
            .reply_base(),
            "https://smba.trafficmanager.net/emea/"
        );
    }
}
