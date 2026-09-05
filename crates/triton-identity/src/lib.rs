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

/// The literal sentinel an [`OidcConfig::issuer`] carries in **Entra
/// multi-tenant** mode. It is never compared to a token `iss` directly —
/// [`issuer_matches`] recognises it and matches any concrete
/// `https://login.microsoftonline.com/<tid>/v2.0`, and [`OidcVerifier::verify`]
/// pins the token's OWN concrete issuer instead.
pub const ENTRA_MULTI_TENANT_ISSUER: &str = "https://login.microsoftonline.com/{tenantid}/v2.0";

/// Entra's global (multi-tenant) signing keys. Every tenant's tokens are
/// signed by the SAME key set, so the multi-tenant verifier pins this one
/// JWKS instead of discovering per-tenant.
const ENTRA_COMMON_JWKS_URL: &str =
    "https://login.microsoftonline.com/organizations/discovery/v2.0/keys";

/// Multi-tenant Entra (Azure AD) acceptance policy (ADR-0021, #673/#675).
///
/// A shared, multi-tenant Entra app issues a token per **customer tenant**
/// whose `iss` is `https://login.microsoftonline.com/<tid>/v2.0` — a
/// different issuer per customer. Exact `(issuer, audience)` pinning cannot
/// express that, so this mode instead accepts any Entra tenant issuer whose
/// `tid` is **allow-listed** here, and maps it to the customer's data tenant.
/// The `tenant_map` is the single entitlement/isolation gate: only listed
/// `tid`s are accepted, and `Principal::tenant` becomes the mapped value.
#[derive(Clone, Debug)]
pub struct EntraMultiTenant {
    /// `tid` (lowercased Entra tenant GUID) → data-tenant name.
    pub tenant_map: HashMap<String, String>,
}

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
    /// When `Some`, this verifier runs in **Entra multi-tenant** mode
    /// (ADR-0021): the token's own tenant-scoped issuer is verified (not
    /// `issuer`, which is the [`ENTRA_MULTI_TENANT_ISSUER`] sentinel), the
    /// `tid` must be allow-listed, and `Principal::tenant` is the mapped
    /// tenant. `None` = ordinary single-issuer verification.
    pub multi_tenant_entra: Option<EntraMultiTenant>,
}

impl OidcConfig {
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            jwks_url: None,
            refresh_interval: Duration::from_secs(30),
            multi_tenant_entra: None,
        }
    }

    /// Pin the JWKS document URL, bypassing OIDC discovery (#100).
    pub fn with_jwks_url(mut self, jwks_url: impl Into<String>) -> Self {
        self.jwks_url = Some(jwks_url.into());
        self
    }

    /// Build an **Entra multi-tenant** verifier (ADR-0021): accepts any
    /// allow-listed Entra tenant for `audience`, pinning Entra's global JWKS.
    /// The `issuer` is the sentinel template; the real per-token issuer is
    /// validated in [`OidcVerifier::verify`].
    pub fn entra_multi_tenant(
        audience: impl Into<String>,
        tenant_map: HashMap<String, String>,
    ) -> Self {
        Self {
            issuer: ENTRA_MULTI_TENANT_ISSUER.to_string(),
            audience: audience.into(),
            jwks_url: Some(ENTRA_COMMON_JWKS_URL.to_string()),
            refresh_interval: Duration::from_secs(30),
            multi_tenant_entra: Some(EntraMultiTenant { tenant_map }),
        }
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
    /// The issuer this verifier is configured to trust. Exposed so a
    /// multi-issuer caller can pick the right verifier for a token
    /// BEFORE verifying it (see [`unverified_issuer`]) — the config
    /// value, never anything read out of a token.
    pub fn issuer(&self) -> &str {
        &self.config.issuer
    }

    /// The audience this verifier requires.
    pub fn audience(&self) -> &str {
        &self.config.audience
    }

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

        // Entra multi-tenant (ADR-0021): the accepted issuer is the token's
        // OWN tenant-scoped issuer — `self.config.issuer` here is the
        // `{tenantid}` sentinel, never compared to a real token. Resolve and
        // ALLOW-LIST the tid BEFORE any key/network work, so an unknown tenant
        // is rejected cheaply and never triggers a JWKS fetch.
        let mt_ctx = match &self.config.multi_tenant_entra {
            Some(mt) => {
                let iss = unverified_issuer(raw_token)
                    .ok_or_else(|| TritonError::Auth("token has no readable iss".into()))?;
                let tid = entra_tid_from_issuer(&iss).ok_or_else(|| {
                    TritonError::Auth(format!(
                        "issuer {iss} is not a tenant-scoped Entra v2 issuer"
                    ))
                })?;
                let tenant =
                    mt.tenant_map.get(&tid).cloned().ok_or_else(|| {
                        TritonError::Auth(format!("tid {tid} is not allow-listed"))
                    })?;
                Some((iss, tid, tenant))
            }
            None => None,
        };

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
        // Multi-tenant: pin the token's CONCRETE issuer (already allow-listed
        // above); single-tenant: pin the configured literal issuer.
        match &mt_ctx {
            Some((iss, _, _)) => validation.set_issuer(&[iss.as_str()]),
            None => validation.set_issuer(&[self.config.issuer.as_str()]),
        }
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);

        let token = decode::<TokenClaims>(raw_token, &key, &validation)
            .map_err(|e| TritonError::Auth(format!("JWT verification failed: {e}")))?;
        let claims = token.claims;
        let scopes = claims.scopes();
        let groups = claims.groups();
        // Tenant: multi-tenant mode derives it from the allow-listed tid (and
        // enforces the anti-mix-up `tid`-claim == issuer-tenant check on the
        // now signature-verified token); single-tenant uses the `tenant` claim.
        let tenant = match &mt_ctx {
            Some((_, tid, tenant)) => {
                if claims.tid.as_deref() != Some(tid.as_str()) {
                    return Err(TritonError::Auth(
                        "tid claim does not match the token issuer's tenant".into(),
                    ));
                }
                tenant.clone()
            }
            None => claims.tenant.clone().unwrap_or_else(|| "-".to_string()),
        };
        Ok(Principal {
            sub: claims.sub,
            scopes,
            groups,
            tenant,
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

/// Accept a claim that is either a single (space-delimited) string or a
/// JSON array of strings, normalizing to `Option<Vec<String>>`. The
/// space-split is the OAuth2 `scope` convention and is exactly how Entra
/// packs `scp`. See [`TokenClaims::scp`] for why tolerating both matters.
fn de_string_or_seq<'de, D>(de: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrSeq {
        String(String),
        Seq(Vec<String>),
    }
    Ok(Option::<StringOrSeq>::deserialize(de)?.map(|v| match v {
        StringOrSeq::String(s) => s.split_whitespace().map(str::to_string).collect(),
        StringOrSeq::Seq(s) => s,
    }))
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    sub: String,
    #[serde(default)]
    tenant: Option<String>,
    /// Microsoft Entra **tenant id** (`tid`). In multi-tenant mode it is
    /// cross-checked against the tid embedded in the verified issuer
    /// (anti-mix-up), and the allow-list maps it to the data tenant.
    #[serde(default)]
    tid: Option<String>,
    /// OAuth2 RFC 6749 single-string form; whitespace-split into
    /// scopes. Some issuers use the `scp` array form instead.
    #[serde(default)]
    scope: Option<String>,
    /// The `scp` (delegated-scope) claim. Its shape is issuer-dependent
    /// and we accept BOTH: Microsoft Entra sends a **space-delimited
    /// string** (`"access_as_user"`), while some other issuers send a
    /// JSON array. Deserializing only the array form made every Entra
    /// delegated token fail verification with `invalid type: string …,
    /// expected a sequence` → a bare 401, which surfaced in Copilot
    /// Studio as an opaque `SystemError` (2026-08-30).
    #[serde(default, deserialize_with = "de_string_or_seq")]
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

/// Read the `iss` claim out of a JWT **without verifying anything**.
///
/// This exists for exactly one purpose: choosing WHICH configured
/// verifier should get a token when several issuers are accepted. The
/// value it returns is attacker-controlled and must never be used as
/// evidence of anything. Nothing is trusted on the basis of this
/// function's output — the selected verifier still checks the
/// signature, `iss`, `aud`, `exp` and the algorithm allowlist, so a
/// token claiming `iss: A` merely gets *offered* to A's verifier and
/// is rejected there unless it genuinely came from A.
///
/// Returns `None` for anything that is not a well-formed JWT payload,
/// which the caller treats as "no verifier matches" — i.e. rejection.
pub fn unverified_issuer(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("iss")?.as_str().map(str::to_string)
}

/// Compare two issuer strings the way OIDC deployments actually vary:
/// a trailing slash is not a different issuer. Everything else is an
/// exact, case-sensitive match — issuers are URLs, and being lax here
/// would widen the trust boundary rather than merely tidy it.
///
/// The ONE controlled exception is the Entra multi-tenant sentinel
/// [`ENTRA_MULTI_TENANT_ISSUER`] (ADR-0021): a verifier configured with it
/// is *selected* for any concrete `https://login.microsoftonline.com/<tid>/v2.0`
/// issuer whose `<tid>` is a well-formed Entra tenant GUID. This only
/// **routes** the token to that verifier — the verifier then still pins the
/// concrete issuer, checks the signature/audience/exp, and allow-lists the
/// tid, so selection never grants trust (same contract as [`unverified_issuer`]).
pub fn issuer_matches(configured: &str, from_token: &str) -> bool {
    if configured == ENTRA_MULTI_TENANT_ISSUER {
        return entra_tid_from_issuer(from_token.trim_end_matches('/')).is_some();
    }
    configured.trim_end_matches('/') == from_token.trim_end_matches('/')
}

/// Extract the lowercased Entra tenant id (`tid`) from a concrete v2 issuer
/// `https://login.microsoftonline.com/<tid>/v2.0`, or `None` if `iss` is not
/// exactly that shape with a well-formed tenant GUID. Strict by construction:
/// the host must be exactly `login.microsoftonline.com` (a look-alike like
/// `login.microsoftonline.com.evil` fails), and `<tid>` must be an
/// `8-4-4-4-12` lowercase-hex GUID.
pub(crate) fn entra_tid_from_issuer(iss: &str) -> Option<String> {
    let mid = iss
        .strip_prefix("https://login.microsoftonline.com/")?
        .strip_suffix("/v2.0")?;
    is_entra_guid(mid).then(|| mid.to_ascii_lowercase())
}

/// A canonical Entra tenant GUID: `8-4-4-4-12` hex, hyphen-separated. We
/// accept either case for robustness but the caller lowercases for the map.
fn is_entra_guid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == groups.len()
        && parts
            .iter()
            .zip(groups)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

// ---- Google opaque access-token introspection (Gemini Enterprise / A2A) ----
//
// Gemini Enterprise forwards Google OAuth **access tokens**, which are opaque
// (not JWTs), so they cannot be verified with a local key. Google validates
// them at its tokeninfo endpoint and returns the audience, email, scopes and
// expiry; we enforce audience + hosted-domain + expiry (fail-closed) and cache
// the result briefly. Opt-in via `GoogleAccessTokenVerifier`; wired as a
// fallback for non-JWT bearers in the HTTP identity middleware.

/// Default Google tokeninfo endpoint. Overridable in tests.
const GOOGLE_TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";

/// The subset of Google's tokeninfo response we consume. `email_verified` and
/// `exp` are typed as `Value` because tokeninfo returns them as JSON strings
/// (e.g. `"true"`, `"1788600000"`) while other Google surfaces use native
/// bool/number — accept both rather than fail to deserialize.
#[derive(Debug, Deserialize, Default)]
struct GoogleTokenInfo {
    aud: Option<String>,
    email: Option<String>,
    email_verified: Option<serde_json::Value>,
    exp: Option<serde_json::Value>,
    scope: Option<String>,
    /// Google Workspace hosted domain, when present.
    hd: Option<String>,
}

fn json_is_true(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            t.eq_ignore_ascii_case("true") || t == "1"
        }
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

fn json_as_i64(v: Option<&serde_json::Value>) -> Option<i64> {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            t.parse::<i64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f as i64))
        }
        _ => None,
    }
}

/// Identity distilled from a validated tokeninfo response.
#[derive(Debug)]
struct GoogleIdentity {
    sub: String,
    scopes: Vec<String>,
    /// Token expiry (unix secs) — used to bound the cache TTL.
    exp: i64,
}

/// Pure, fail-closed validation of a tokeninfo response against the configured
/// audience and hosted domain. Every missing/invalid field is a rejection.
/// Unit-tested without any HTTP.
fn validate_google_tokeninfo(
    info: &GoogleTokenInfo,
    audience: &str,
    allowed_hd: Option<&str>,
    now_unix: i64,
) -> Result<GoogleIdentity, TritonError> {
    // (1) Audience MUST equal our client id. Without this, an access token
    // minted for ANY Google client would be accepted here — the confused-
    // deputy / token-substitution hole. This is the load-bearing check.
    if info.aud.as_deref() != Some(audience) {
        return Err(TritonError::Auth(
            "google access token audience mismatch".into(),
        ));
    }
    // (2) A verified email is required.
    let email = info
        .email
        .as_deref()
        .filter(|e| !e.is_empty())
        .ok_or_else(|| TritonError::Auth("google access token has no email".into()))?;
    if !json_is_true(info.email_verified.as_ref()) {
        return Err(TritonError::Auth(
            "google access token email is not verified".into(),
        ));
    }
    // (3) Hosted-domain boundary (mirrors ADR-0017 pair-1). Prefer the `hd`
    // claim; fall back to the email domain when `hd` is absent.
    if let Some(hd) = allowed_hd {
        // The `hd` (Workspace hosted-domain) claim is AUTHORITATIVE when
        // present: a conflicting `hd` is rejected even if the email domain
        // matches (aliases/vanity domains make the email domain only a
        // heuristic). Fall back to the email domain ONLY when `hd` is absent.
        // Case-insensitive (DNS labels are).
        let domain_ok = match info.hd.as_deref() {
            Some(h) => h.trim().eq_ignore_ascii_case(hd),
            None => email
                .rsplit('@')
                .next()
                .is_some_and(|d| d.eq_ignore_ascii_case(hd)),
        };
        if !domain_ok {
            return Err(TritonError::Auth(
                "google access token is outside the allowed domain".into(),
            ));
        }
    }
    // (4) Expiry.
    let exp = json_as_i64(info.exp.as_ref())
        .ok_or_else(|| TritonError::Auth("google access token has no exp".into()))?;
    if exp <= now_unix {
        return Err(TritonError::Auth("google access token is expired".into()));
    }
    let scopes = info
        .scope
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    Ok(GoogleIdentity {
        sub: email.to_string(),
        scopes,
        exp,
    })
}

struct GoogleCacheEntry {
    sub: String,
    scopes: Vec<String>,
    expires: Instant,
}

/// Verifier for Google **opaque** OAuth access tokens forwarded over A2A
/// (Gemini Enterprise). Validation is by introspection against Google's
/// tokeninfo endpoint plus [`validate_google_tokeninfo`]; results are cached by
/// token for up to 5 minutes (bounded) so it is not one network call per
/// request. Fail-closed on every error.
pub struct GoogleAccessTokenVerifier {
    audience: String,
    allowed_hd: Option<String>,
    tokeninfo_url: String,
    http: reqwest::Client,
    cache: RwLock<HashMap<String, GoogleCacheEntry>>,
}

impl GoogleAccessTokenVerifier {
    pub fn new(audience: impl Into<String>, allowed_hd: Option<String>) -> Self {
        Self {
            audience: audience.into(),
            allowed_hd: allowed_hd
                .map(|d| d.trim().to_ascii_lowercase())
                .filter(|d| !d.is_empty()),
            tokeninfo_url: GOOGLE_TOKENINFO_URL.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                // The bearer rides in the query string, so a redirect would
                // replay it to the target — never follow one (crew F3).
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The audience this verifier requires (the Google OAuth client id).
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// Test-only: point introspection at a local fake tokeninfo server.
    /// Compiled out of release builds so it can never be wired to an env
    /// var (SSRF footgun — crew F9).
    #[cfg(test)]
    pub fn with_tokeninfo_url(mut self, url: impl Into<String>) -> Self {
        self.tokeninfo_url = url.into();
        self
    }

    pub async fn verify(&self, raw_token: &str) -> Result<Principal, TritonError> {
        if let Some((sub, scopes)) = self.cache_get(raw_token).await {
            return Ok(self.principal(sub, scopes, raw_token));
        }
        let resp = self
            .http
            .get(&self.tokeninfo_url)
            .query(&[("access_token", raw_token)])
            .send()
            .await
            .map_err(|e| {
                // reqwest's Display embeds the request URL, which carries the
                // `?access_token=` bearer — strip it before it reaches logs or
                // the client error body (crew F2).
                tracing::warn!(error = %e.without_url(), "google tokeninfo request failed");
                TritonError::Auth("google tokeninfo request failed".into())
            })?;
        if !resp.status().is_success() {
            // 400 = invalid/expired token; any non-2xx is a fail-closed reject.
            return Err(TritonError::Auth(
                "google access token rejected by tokeninfo".into(),
            ));
        }
        let info: GoogleTokenInfo = resp.json().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), "google tokeninfo decode failed");
            TritonError::Auth("google tokeninfo decode failed".into())
        })?;
        let now = now_unix();
        let id = validate_google_tokeninfo(&info, &self.audience, self.allowed_hd.as_deref(), now)?;
        // Cache for min(remaining lifetime, 5 min). A token revoked at Google
        // stays admitted for up to this window (crew F6 — documented tradeoff;
        // there is no live revocation channel).
        let ttl = (id.exp - now).clamp(0, 300) as u64;
        self.cache_put(raw_token, &id, Duration::from_secs(ttl))
            .await;
        Ok(self.principal(id.sub, id.scopes, raw_token))
    }

    fn principal(&self, sub: String, scopes: Vec<String>, raw_token: &str) -> Principal {
        Principal {
            sub,
            scopes,
            groups: Vec::new(),
            // Google access-token callers carry no tenant claim; the agent's
            // own escurel signer applies its default tenant (single-tenant).
            tenant: "-".to_string(),
            raw_token: raw_token.to_string(),
            trace_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    async fn cache_get(&self, token: &str) -> Option<(String, Vec<String>)> {
        let cache = self.cache.read().await;
        let e = cache.get(token)?;
        (e.expires > Instant::now()).then(|| (e.sub.clone(), e.scopes.clone()))
    }

    async fn cache_put(&self, token: &str, id: &GoogleIdentity, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let mut cache = self.cache.write().await;
        let now = Instant::now();
        cache.retain(|_, e| e.expires > now);
        // Bounded: never grow past the cap (correctness is unaffected — a
        // cache miss just re-introspects).
        if cache.len() >= 1024 {
            return;
        }
        cache.insert(
            token.to_string(),
            GoogleCacheEntry {
                sub: id.sub.clone(),
                scopes: id.scopes.clone(),
                expires: now + ttl,
            },
        );
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod multi_issuer_tests {
    use super::*;
    use base64::Engine as _;

    fn jwt_with_payload(payload: &serde_json::Value) -> String {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"RS256","typ":"JWT"}"#),
            b64(payload.to_string().as_bytes()),
            b64(b"not-a-real-signature")
        )
    }

    #[test]
    fn reads_the_iss_claim_without_verifying() {
        let t = jwt_with_payload(&serde_json::json!({
            "iss": "https://accounts.google.com",
            "aud": "client-1",
        }));
        assert_eq!(
            unverified_issuer(&t).as_deref(),
            Some("https://accounts.google.com")
        );
    }

    #[test]
    fn malformed_tokens_yield_no_issuer_rather_than_panicking() {
        // Each of these reaches the peek from a real request path, so
        // none may panic; `None` means "no verifier matches" upstream,
        // which is a rejection.
        for bad in [
            "",
            "not-a-jwt",
            "only.two",
            "a.!!!not-base64!!!.c",
            // valid base64, not JSON
            "a.aGVsbG8.c",
            // valid JSON, no iss
            "a.eyJhdWQiOiJ4In0.c",
            // iss present but not a string
            "a.eyJpc3MiOjQyfQ.c",
        ] {
            assert_eq!(unverified_issuer(bad), None, "input {bad:?}");
        }
    }

    #[test]
    fn issuer_match_tolerates_only_a_trailing_slash() {
        assert!(issuer_matches("https://issuer.test", "https://issuer.test"));
        assert!(issuer_matches(
            "https://issuer.test/",
            "https://issuer.test"
        ));
        assert!(issuer_matches(
            "https://issuer.test",
            "https://issuer.test/"
        ));

        // Everything else is a different issuer. A lax comparison here
        // would widen the trust boundary, not tidy it.
        assert!(!issuer_matches(
            "https://issuer.test",
            "https://issuer.test.evil"
        ));
        assert!(!issuer_matches("https://issuer.test", "http://issuer.test"));
        assert!(!issuer_matches(
            "https://issuer.test",
            "https://ISSUER.test"
        ));
        assert!(!issuer_matches(
            "https://issuer.test",
            "https://issuer.test/x"
        ));
        assert!(!issuer_matches("https://issuer.test", ""));
    }

    #[test]
    fn scp_claim_accepts_both_entra_string_and_array_forms() {
        // Entra (delegated) packs scp as a space-delimited STRING.
        // Deserializing this used to fail with `invalid type: string …,
        // expected a sequence`, i.e. a bare 401 → Copilot "SystemError".
        let entra: TokenClaims = serde_json::from_value(serde_json::json!({
            "sub": "u1",
            "scp": "access_as_user User.Read"
        }))
        .expect("Entra string-form scp must deserialize");
        assert_eq!(
            entra.scopes(),
            vec!["access_as_user".to_string(), "User.Read".to_string()]
        );

        // The array form (some OIDC issuers) still works.
        let arr: TokenClaims = serde_json::from_value(serde_json::json!({
            "sub": "u2",
            "scp": ["a", "b"]
        }))
        .expect("array-form scp must deserialize");
        assert_eq!(arr.scopes(), vec!["a".to_string(), "b".to_string()]);

        // Absent scp falls back to the OAuth `scope` string.
        let scope_only: TokenClaims = serde_json::from_value(serde_json::json!({
            "sub": "u3",
            "scope": "x y"
        }))
        .expect("scope-only must deserialize");
        assert_eq!(scope_only.scopes(), vec!["x".to_string(), "y".to_string()]);

        // Neither present ⇒ empty, no error.
        let none: TokenClaims = serde_json::from_value(serde_json::json!({ "sub": "u4" }))
            .expect("bare claims must deserialize");
        assert!(none.scopes().is_empty());
    }
}

#[cfg(test)]
mod entra_multitenant_tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::traits::PublicKeyParts;
    use rsa::{RsaPrivateKey, RsaPublicKey};
    use std::time::{SystemTime, UNIX_EPOCH};

    const KID: &str = "mt-test-key";
    const TID_A: &str = "28c0071d-815c-4ace-a3b5-9a28bde005fd";
    const TID_B: &str = "11112222-3333-4444-5555-666677778888";

    fn iss(tid: &str) -> String {
        format!("https://login.microsoftonline.com/{tid}/v2.0")
    }

    /// Throwaway RSA keypair + its public JWKS (one `kid`).
    fn keypair() -> (String, serde_json::Value) {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
        let pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .expect("pem")
            .to_string();
        let public = RsaPublicKey::from(&private);
        let b64 = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
        let jwks = serde_json::json!({ "keys": [{
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KID,
            "n": b64(&public.n().to_bytes_be()),
            "e": b64(&public.e().to_bytes_be()),
        }] });
        (pem, jwks)
    }

    fn sign(pem: &str, iss: &str, aud: &str, tid: Option<&str>) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut claims = serde_json::json!({
            "sub": "user-1", "iss": iss, "aud": aud, "exp": now + 300, "iat": now,
        });
        if let Some(t) = tid {
            claims["tid"] = serde_json::json!(t);
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KID.to_string());
        let key = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("enc key");
        encode(&header, &claims, &key).expect("sign")
    }

    /// A multi-tenant verifier with the JWKS pre-installed (so `verify` never
    /// makes a network fetch — the pinned Entra JWKS is stubbed by the cache).
    async fn verifier(map: &[(&str, &str)], jwks: &serde_json::Value) -> OidcVerifier {
        let tenant_map: HashMap<String, String> = map
            .iter()
            .map(|(t, n)| (t.to_string(), n.to_string()))
            .collect();
        let v = OidcVerifier::new(OidcConfig::entra_multi_tenant("api://our-app", tenant_map));
        let set: JwkSet = serde_json::from_value(jwks.clone()).unwrap();
        v.install_keys(&set).await;
        v
    }

    #[tokio::test]
    async fn two_allow_listed_tenants_verify_and_map_to_their_data_tenant() {
        let (pem, jwks) = keypair();
        let v = verifier(&[(TID_A, "acme"), (TID_B, "globex")], &jwks).await;

        let a = v
            .verify(&sign(&pem, &iss(TID_A), "api://our-app", Some(TID_A)))
            .await
            .expect("tenant A verifies");
        assert_eq!(a.tenant, "acme");
        assert_eq!(a.sub, "user-1");

        let b = v
            .verify(&sign(&pem, &iss(TID_B), "api://our-app", Some(TID_B)))
            .await
            .expect("tenant B verifies");
        assert_eq!(b.tenant, "globex");
    }

    #[tokio::test]
    async fn unlisted_tid_is_rejected() {
        let (pem, jwks) = keypair();
        let v = verifier(&[(TID_A, "acme")], &jwks).await;
        let err = v
            .verify(&sign(&pem, &iss(TID_B), "api://our-app", Some(TID_B)))
            .await
            .expect_err("un-allow-listed tenant must be rejected");
        assert!(format!("{err}").contains("not allow-listed"), "{err}");
    }

    #[tokio::test]
    async fn wrong_audience_is_rejected() {
        let (pem, jwks) = keypair();
        let v = verifier(&[(TID_A, "acme")], &jwks).await;
        let err = v
            .verify(&sign(&pem, &iss(TID_A), "api://someone-else", Some(TID_A)))
            .await
            .expect_err("wrong audience must be rejected");
        assert!(matches!(err, TritonError::Auth(_)), "{err}");
    }

    #[tokio::test]
    async fn tid_claim_must_match_issuer_tenant() {
        let (pem, jwks) = keypair();
        let v = verifier(&[(TID_A, "acme"), (TID_B, "globex")], &jwks).await;
        // Issuer says tenant A (allow-listed) but the `tid` CLAIM says B → mix-up.
        let err = v
            .verify(&sign(&pem, &iss(TID_A), "api://our-app", Some(TID_B)))
            .await
            .expect_err("tid/issuer mismatch must be rejected");
        assert!(format!("{err}").contains("does not match"), "{err}");
        // Missing `tid` claim entirely → also rejected.
        let err = v
            .verify(&sign(&pem, &iss(TID_A), "api://our-app", None))
            .await
            .expect_err("missing tid claim must be rejected");
        assert!(format!("{err}").contains("does not match"), "{err}");
    }

    #[test]
    fn issuer_template_matching_and_guid_parsing() {
        // The sentinel routes any well-formed concrete Entra issuer...
        assert!(issuer_matches(ENTRA_MULTI_TENANT_ISSUER, &iss(TID_A)));
        // ...but not a look-alike host, a non-GUID segment, or a foreign issuer.
        assert!(!issuer_matches(
            ENTRA_MULTI_TENANT_ISSUER,
            "https://login.microsoftonline.com.evil/28c0071d-815c-4ace-a3b5-9a28bde005fd/v2.0"
        ));
        assert!(!issuer_matches(
            ENTRA_MULTI_TENANT_ISSUER,
            "https://login.microsoftonline.com/not-a-guid/v2.0"
        ));
        assert!(!issuer_matches(
            ENTRA_MULTI_TENANT_ISSUER,
            "https://accounts.google.com"
        ));
        // A non-template issuer keeps exact (trailing-slash-tolerant) matching.
        assert!(issuer_matches(
            "https://issuer.test",
            "https://issuer.test/"
        ));
        assert!(!issuer_matches("https://issuer.test", &iss(TID_A)));

        assert_eq!(entra_tid_from_issuer(&iss(TID_A)).as_deref(), Some(TID_A));
        assert_eq!(
            entra_tid_from_issuer(
                "https://login.microsoftonline.com.evil/28c0071d-815c-4ace-a3b5-9a28bde005fd/v2.0"
            ),
            None
        );
        assert_eq!(
            entra_tid_from_issuer("https://login.microsoftonline.com/28c0071d/v2.0"),
            None
        );
        // An upper-case GUID is accepted but normalised to lowercase.
        let upper = "28C0071D-815C-4ACE-A3B5-9A28BDE005FD";
        assert_eq!(entra_tid_from_issuer(&iss(upper)).as_deref(), Some(TID_A));
    }
}

#[cfg(test)]
mod google_access_token_tests {
    use super::*;
    use serde_json::json;

    const AUD: &str = "741034231082-94ns.apps.googleusercontent.com";
    const HD: &str = "data-zoo.de";

    fn info(overrides: serde_json::Value) -> GoogleTokenInfo {
        // Base: a valid token for AUD, verified email at HD, far-future exp.
        let mut base = json!({
            "aud": AUD,
            "email": "jr@data-zoo.de",
            "email_verified": "true",
            "exp": "9999999999",
            "scope": "openid email profile",
            "hd": "data-zoo.de",
        });
        for (k, v) in overrides.as_object().unwrap() {
            if v.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn valid_token_yields_email_subject_and_scopes() {
        let id = validate_google_tokeninfo(&info(json!({})), AUD, Some(HD), 1000).unwrap();
        assert_eq!(id.sub, "jr@data-zoo.de");
        assert_eq!(id.scopes, vec!["openid", "email", "profile"]);
    }

    #[test]
    fn audience_mismatch_is_rejected() {
        let err = validate_google_tokeninfo(
            &info(json!({"aud": "someone-else.apps.googleusercontent.com"})),
            AUD,
            Some(HD),
            1000,
        )
        .expect_err("wrong aud must fail");
        assert!(format!("{err}").contains("audience mismatch"), "{err}");
    }

    #[test]
    fn missing_or_unverified_email_is_rejected() {
        assert!(
            validate_google_tokeninfo(&info(json!({"email": null})), AUD, Some(HD), 1000).is_err()
        );
        assert!(
            validate_google_tokeninfo(
                &info(json!({"email_verified": "false"})),
                AUD,
                Some(HD),
                1000
            )
            .is_err()
        );
        assert!(
            validate_google_tokeninfo(&info(json!({"email_verified": null})), AUD, Some(HD), 1000)
                .is_err()
        );
    }

    #[test]
    fn domain_enforced_via_hd_or_email_fallback() {
        // hd absent but email domain matches → ok.
        assert!(validate_google_tokeninfo(&info(json!({"hd": null})), AUD, Some(HD), 1000).is_ok());
        // wrong domain (both hd and email) → rejected.
        let err = validate_google_tokeninfo(
            &info(json!({"hd": "evil.com", "email": "x@evil.com"})),
            AUD,
            Some(HD),
            1000,
        )
        .expect_err("wrong domain must fail");
        assert!(format!("{err}").contains("allowed domain"), "{err}");
        // CONFLICTING hd (present) wins over a matching email domain → rejected
        // (crew F1: hd is authoritative when present).
        let err = validate_google_tokeninfo(
            &info(json!({"hd": "evil.com", "email": "alice@data-zoo.de"})),
            AUD,
            Some(HD),
            1000,
        )
        .expect_err("conflicting hd must fail even with a matching email");
        assert!(format!("{err}").contains("allowed domain"), "{err}");
        // Case-insensitive hosted-domain match.
        assert!(
            validate_google_tokeninfo(
                &info(json!({"hd": "DATA-ZOO.DE"})),
                AUD,
                Some("data-zoo.de"),
                1000
            )
            .is_ok()
        );
        // No allowed_hd configured → any domain passes (still needs aud+email).
        assert!(
            validate_google_tokeninfo(
                &info(json!({"hd": null, "email": "x@other.com"})),
                AUD,
                None,
                1000
            )
            .is_ok()
        );
    }

    #[test]
    fn expired_token_is_rejected_and_exp_accepts_string_or_number() {
        // Expired (exp <= now).
        let err = validate_google_tokeninfo(&info(json!({"exp": "500"})), AUD, Some(HD), 1000)
            .expect_err("expired must fail");
        assert!(format!("{err}").contains("expired"), "{err}");
        // exp as a native number is also accepted.
        assert!(
            validate_google_tokeninfo(&info(json!({"exp": 9999999999i64})), AUD, Some(HD), 1000)
                .is_ok()
        );
        // missing exp → rejected.
        assert!(
            validate_google_tokeninfo(&info(json!({"exp": null})), AUD, Some(HD), 1000).is_err()
        );
    }

    #[test]
    fn email_verified_accepts_bool_true_too() {
        assert!(
            validate_google_tokeninfo(&info(json!({"email_verified": true})), AUD, Some(HD), 1000)
                .is_ok()
        );
    }

    async fn spawn_tokeninfo(status: u16, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let reason = if status == 200 { "OK" } else { "Bad Request" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn verify_admits_valid_opaque_token_and_caches() {
        let body = format!(
            r#"{{"aud":"{AUD}","email":"jr@data-zoo.de","email_verified":"true","exp":"9999999999","scope":"openid email","hd":"data-zoo.de"}}"#
        );
        let url = spawn_tokeninfo(200, body).await;
        let v = GoogleAccessTokenVerifier::new(AUD, Some(HD.into())).with_tokeninfo_url(url);
        let p = v.verify("ya29.opaque").await.expect("valid token admitted");
        assert_eq!(p.sub, "jr@data-zoo.de");
        assert_eq!(p.tenant, "-");
        // Second call is a cache hit (still Ok).
        let p2 = v.verify("ya29.opaque").await.expect("cache hit");
        assert_eq!(p2.sub, "jr@data-zoo.de");
    }

    #[tokio::test]
    async fn error_string_never_contains_the_raw_token() {
        // Point at a closed port so the send fails; the reqwest URL (which
        // carries ?access_token=<secret>) must not leak into the error (F2).
        let v = GoogleAccessTokenVerifier::new(AUD, Some(HD.into()))
            .with_tokeninfo_url("http://127.0.0.1:1/tokeninfo");
        let secret = "ya29.SUPER-SECRET-BEARER-VALUE";
        let err = v
            .verify(secret)
            .await
            .expect_err("closed port must fail closed");
        assert!(
            !format!("{err}").contains("SUPER-SECRET-BEARER-VALUE"),
            "token leaked into error: {err}"
        );
    }

    #[tokio::test]
    async fn verify_fails_closed_on_tokeninfo_non_2xx() {
        let url = spawn_tokeninfo(400, r#"{"error":"invalid_token"}"#.to_string()).await;
        let v = GoogleAccessTokenVerifier::new(AUD, Some(HD.into())).with_tokeninfo_url(url);
        let err = v.verify("bad").await.expect_err("400 must fail closed");
        assert!(matches!(err, TritonError::Auth(_)), "{err}");
    }

    #[tokio::test]
    async fn verify_rejects_wrong_audience_from_live_response() {
        let body = format!(
            r#"{{"aud":"attacker.apps.googleusercontent.com","email":"jr@data-zoo.de","email_verified":"true","exp":"9999999999","hd":"data-zoo.de"}}"#
        );
        let url = spawn_tokeninfo(200, body).await;
        let v = GoogleAccessTokenVerifier::new(AUD, Some(HD.into())).with_tokeninfo_url(url);
        let err = v
            .verify("token-for-other-app")
            .await
            .expect_err("aud mismatch must fail");
        assert!(format!("{err}").contains("audience mismatch"), "{err}");
    }
}
