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
pub fn issuer_matches(configured: &str, from_token: &str) -> bool {
    configured.trim_end_matches('/') == from_token.trim_end_matches('/')
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
