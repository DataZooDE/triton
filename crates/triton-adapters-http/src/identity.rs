//! Identity boundary for the HTTP trio. Holds the optional
//! [`OidcVerifier`] (FR-I-1..3), the optional `X-Forwarded-Email`
//! trust path (issue #67 / ADR-0011 sidecar pattern), and the cfg-
//! gated dev-token fallback (ADR-10, FR-I-5). Production builds
//! (`--no-default-features`) reject any non-OIDC bearer at compile
//! time.
//!
//! Precedence (highest to lowest):
//!   1. OIDC verifier — if configured, it is the **only** accepted
//!      identity. Even a build with `dev-token` compiled in MUST
//!      reject the dev token when OIDC is live, so an accidental
//!      env-var omission doesn't open a backdoor. The forwarded-auth
//!      fast-path is also disabled in this mode: real PKCE/Bearer is
//!      the source of truth.
//!   2. `X-Forwarded-Email` — admitted only when
//!      `trust_forwarded_auth` is `true` (opt-in via
//!      `TRITON_TRUST_FORWARDED_AUTH`) AND the OIDC verifier is OFF.
//!      Matches the auth-portal-dz idiom where an `oauth2-proxy`
//!      sidecar authenticates the operator against Vault's `ops`
//!      realm and forwards the request on the alloc's loopback.
//!   3. `Authorization: Bearer <token>` — falls through to the
//!      `dev-token` literal accepted when `dev-token` is compiled in
//!      AND no OIDC verifier is configured. Production builds with
//!      `--no-default-features` reject every bearer that doesn't
//!      pass OIDC.

use std::sync::Arc;

use axum::http::HeaderName;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use triton_core::{Principal, TritonError};
use triton_identity::{GoogleAccessTokenVerifier, OidcVerifier, issuer_matches, unverified_issuer};

/// Header set by the upstream `oauth2-proxy` sidecar
/// (`--pass-user-headers=true`). Matches what `auth-portal-dz`
/// relies on. Lowercase to match `http::HeaderName::from_static`.
const FORWARDED_EMAIL_HEADER: HeaderName = HeaderName::from_static("x-forwarded-email");

/// The default accepted dev token when `TRITON_DEV_TOKEN` is unset —
/// preserves the historical literal so existing dev workflows and tests
/// keep working. Operators set `TRITON_DEV_TOKEN` to a non-guessable
/// secret (or empty, to disable the dev-token path entirely).
const DEFAULT_DEV_TOKEN: &str = "dev-token";

#[derive(Clone)]
pub struct IdentityProvider {
    /// Accepted OIDC issuer/audience pairs, in configuration order.
    /// Empty = no OIDC boundary (dev-token / forwarded-auth paths).
    ///
    /// More than one entry is how a single agent serves callers from
    /// different identity providers at once — e.g. Google for humans and
    /// Entra for a Microsoft agent platform. They are alternatives, not
    /// layers: a token satisfying ANY configured pair is accepted, and
    /// each pair is as strong a boundary as it would be alone.
    oidc: Vec<Arc<OidcVerifier>>,
    /// Whether `X-Forwarded-Email` should be honoured when present.
    /// Wired from `TRITON_TRUST_FORWARDED_AUTH`. ONLY safe when
    /// Triton binds loopback inside a Nomad alloc and the only thing
    /// that can set the header is a sidecar in the shared netns.
    trust_forwarded_auth: bool,
    /// The accepted dev token (`TRITON_DEV_TOKEN`, default `dev-token`).
    /// Only consulted on the dev-token path — feature-gated AND only when
    /// no OIDC verifier is configured (OIDC always wins, ADR-10). An empty
    /// value disables the dev-token path entirely (rejects every bearer):
    /// a kill-switch even in a `dev-token` build.
    dev_token: String,
    /// Optional fallback for **opaque** Google OAuth access tokens (not JWTs)
    /// — what Gemini Enterprise forwards over A2A. Tried ONLY when the bearer
    /// has no readable `iss` (i.e. is not a JWT); JWTs always route to `oidc`.
    /// Validated by introspection (audience + hosted domain, fail-closed).
    google_access: Option<Arc<GoogleAccessTokenVerifier>>,
}

impl IdentityProvider {
    /// Backwards-compatible constructor that disables the
    /// forwarded-auth fast-path. New callers should prefer
    /// [`IdentityProvider::with_forwarded_auth`].
    pub fn new(oidc: Option<Arc<OidcVerifier>>) -> Self {
        Self {
            oidc: oidc.into_iter().collect(),
            trust_forwarded_auth: false,
            dev_token: DEFAULT_DEV_TOKEN.to_string(),
            google_access: None,
        }
    }

    /// Multi-issuer constructor. An empty vec behaves exactly like
    /// `new(None)`; one entry exactly like `new(Some(_))`.
    pub fn with_verifiers(oidc: Vec<Arc<OidcVerifier>>, trust_forwarded_auth: bool) -> Self {
        Self {
            oidc,
            trust_forwarded_auth,
            dev_token: DEFAULT_DEV_TOKEN.to_string(),
            google_access: None,
        }
    }

    /// Constructor that opts into trusting `X-Forwarded-Email` when
    /// set by the co-located oauth2-proxy sidecar (issue #67).
    pub fn with_forwarded_auth(
        oidc: Option<Arc<OidcVerifier>>,
        trust_forwarded_auth: bool,
    ) -> Self {
        Self {
            oidc: oidc.into_iter().collect(),
            trust_forwarded_auth,
            dev_token: DEFAULT_DEV_TOKEN.to_string(),
            google_access: None,
        }
    }

    /// Override the accepted dev token (from `TRITON_DEV_TOKEN`). Builder
    /// so existing constructor call sites — and the ~70 tests that send
    /// `Bearer dev-token` — are unchanged. Empty disables the path.
    pub fn with_dev_token(mut self, token: String) -> Self {
        self.dev_token = token;
        self
    }

    /// Add the opaque Google access-token fallback (Gemini Enterprise / A2A).
    /// Builder so existing call sites are unchanged.
    pub fn with_google_access(mut self, verifier: Arc<GoogleAccessTokenVerifier>) -> Self {
        self.google_access = Some(verifier);
        self
    }

    pub async fn verify(&self, parts: &Parts) -> Result<Principal, TritonError> {
        if !self.oidc.is_empty() || self.google_access.is_some() {
            // OIDC (and/or the opaque Google access-token fallback) live → only
            // that path. The forwarded-auth fast-path is disabled in this mode
            // so a stale `trust_forwarded_auth=true` can never override real
            // PKCE. Including `google_access` here means a host that configures
            // ONLY the opaque fallback still gets it consulted (crew F4) rather
            // than a silently-inert verifier.
            return self.verify_bearer_via_oidc(parts).await;
        }

        // Trust flag set but no header — fall through to the dev-token
        // path so a misconfigured sidecar (no `--pass-user-headers`)
        // doesn't admit anonymous traffic.
        if self.trust_forwarded_auth
            && let Some(email) = forwarded_email(parts)?
        {
            return Ok(forwarded_email_principal(email));
        }

        let token = bearer_from(parts)?;
        verify_dev_or_reject(token, &self.dev_token)
    }

    /// Verify the bearer against whichever configured issuer the token
    /// claims to come from.
    ///
    /// With exactly one verifier this calls it directly — same code path,
    /// same error text as before multi-issuer existed, so a single-pair
    /// deployment cannot regress.
    ///
    /// With several, the token's `iss` is peeked UNVERIFIED purely to
    /// select a verifier; the selected one then performs the full check.
    /// Selection is not trust: a token claiming someone else's issuer is
    /// simply handed to that issuer's verifier, which rejects it. Trying
    /// every verifier instead would be equally safe but would mean N
    /// JWKS lookups per bad token, so an unmatched issuer is rejected
    /// without a network call.
    async fn verify_bearer_via_oidc(&self, parts: &Parts) -> Result<Principal, TritonError> {
        let token = bearer_from(parts)?;
        // Opaque (non-JWT) bearer: no readable `iss` to route on. If a Google
        // access-token verifier is configured, introspect it (Gemini Enterprise
        // forwards opaque Google access tokens over A2A). Checked before the
        // single-verifier shortcut so it works for one- and multi-pair
        // deployments alike. JWTs (with an `iss`) fall through to OIDC routing.
        if unverified_issuer(token).is_none()
            && let Some(g) = &self.google_access
        {
            return g.verify(token).await;
        }
        if let [only] = self.oidc.as_slice() {
            return only.verify(token).await;
        }
        let claimed = unverified_issuer(token).ok_or_else(|| {
            TritonError::Auth("bearer is not a JWT with a readable `iss` claim".into())
        })?;
        let verifier = self
            .oidc
            .iter()
            .find(|v| issuer_matches(v.issuer(), &claimed))
            .ok_or_else(|| {
                // Name the rejected issuer, not the accepted list: the
                // operator can read the accepted pairs from /v1/runtime,
                // and an error is a poor place to enumerate config.
                TritonError::Auth(format!(
                    "no configured issuer accepts tokens from {claimed}"
                ))
            })?;
        verifier.verify(token).await
    }
}

fn bearer_from(parts: &Parts) -> Result<&str, TritonError> {
    let header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or_else(|| TritonError::Auth("missing Authorization header".into()))?
        .to_str()
        .map_err(|_| TritonError::Auth("non-ASCII Authorization header".into()))?;

    header
        .strip_prefix("Bearer ")
        .ok_or_else(|| TritonError::Auth("expected `Bearer <token>`".into()))
        .map(str::trim)
}

/// Read `X-Forwarded-Email` when set; `Err` only on a header that
/// exists but isn't ASCII (an oauth2-proxy bug or a header-injection
/// attempt). Returns `Ok(None)` when the header isn't present at all
/// so the caller can fall through to the Bearer path.
fn forwarded_email(parts: &Parts) -> Result<Option<&str>, TritonError> {
    match parts.headers.get(&FORWARDED_EMAIL_HEADER) {
        Some(v) => {
            let s = v
                .to_str()
                .map_err(|_| TritonError::Auth("non-ASCII X-Forwarded-Email header".into()))?;
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        None => Ok(None),
    }
}

fn forwarded_email_principal(email: &str) -> Principal {
    Principal {
        sub: email.to_string(),
        // The scope mirrors auth-portal-dz's session model — the
        // operator authenticated against Vault's `ops` realm.
        scopes: vec!["sso-ops".to_string()],
        groups: Vec::new(),
        tenant: "ops".to_string(),
        // No raw bearer to forward: the upstream router's Vault
        // OIDC swap is intentionally unavailable on this path. Demo
        // / in-process tools only. Real PKCE end-to-end is tracked
        // separately (issue #67 option B).
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
        sender_ref: None,
    }
}

/// Constant-time string compare that does not leak length through the
/// early return. Mirrors the padded `ct_eq` used for correlation tokens.
#[cfg(feature = "dev-token")]
fn ct_eq_str(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().max(b.len());
    let mut pa = vec![0u8; n];
    let mut pb = vec![0u8; n];
    pa[..a.len()].copy_from_slice(a);
    pb[..b.len()].copy_from_slice(b);
    let content: bool = pa.ct_eq(&pb).into();
    content && a.len() == b.len()
}

#[cfg(feature = "dev-token")]
fn verify_dev_or_reject(token: &str, expected: &str) -> Result<Principal, TritonError> {
    // An empty `expected` disables the dev-token path (kill-switch) and
    // also prevents an empty `Bearer ` from matching an empty token.
    // #282 F10: constant-time, like every other secret compare in the
    // workspace. Length is padded into the comparison so the path taken
    // does not depend on where the tokens first differ.
    if expected.is_empty() || !ct_eq_str(token, expected) {
        return Err(TritonError::Auth("unknown token".into()));
    }
    Ok(Principal {
        sub: "dev-user".into(),
        // #282: the dev principal holds the operator audit scope. The
        // Explorer authenticates with this token, and the rows it exists
        // to show belong to chat senders, not to `tenant: "dev"` — so
        // without it the audit page silently empties. This grants nothing
        // in production: the whole dev-token path is compiled out there
        // (`--no-default-features`, ADR-10).
        scopes: vec!["dev".into(), crate::rest::AUDIT_READ_ALL_SCOPE.into()],
        groups: Vec::new(),
        tenant: "dev".into(),
        raw_token: token.into(),
        trace_id: uuid::Uuid::new_v4().to_string(),
        sender_ref: None,
    })
}

#[cfg(not(feature = "dev-token"))]
fn verify_dev_or_reject(_token: &str, _expected: &str) -> Result<Principal, TritonError> {
    Err(TritonError::Auth(
        "no OIDC verifier configured and dev-token disabled at build time (ADR-10)".into(),
    ))
}
