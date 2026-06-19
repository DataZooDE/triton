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
use triton_identity::OidcVerifier;

/// Header set by the upstream `oauth2-proxy` sidecar
/// (`--pass-user-headers=true`). Matches what `auth-portal-dz`
/// relies on. Lowercase to match `http::HeaderName::from_static`.
const FORWARDED_EMAIL_HEADER: HeaderName = HeaderName::from_static("x-forwarded-email");

#[derive(Clone)]
pub struct IdentityProvider {
    oidc: Option<Arc<OidcVerifier>>,
    /// Whether `X-Forwarded-Email` should be honoured when present.
    /// Wired from `TRITON_TRUST_FORWARDED_AUTH`. ONLY safe when
    /// Triton binds loopback inside a Nomad alloc and the only thing
    /// that can set the header is a sidecar in the shared netns.
    trust_forwarded_auth: bool,
}

impl IdentityProvider {
    /// Backwards-compatible constructor that disables the
    /// forwarded-auth fast-path. New callers should prefer
    /// [`IdentityProvider::with_forwarded_auth`].
    pub fn new(oidc: Option<Arc<OidcVerifier>>) -> Self {
        Self {
            oidc,
            trust_forwarded_auth: false,
        }
    }

    /// Constructor that opts into trusting `X-Forwarded-Email` when
    /// set by the co-located oauth2-proxy sidecar (issue #67).
    pub fn with_forwarded_auth(
        oidc: Option<Arc<OidcVerifier>>,
        trust_forwarded_auth: bool,
    ) -> Self {
        Self {
            oidc,
            trust_forwarded_auth,
        }
    }

    pub async fn verify(&self, parts: &Parts) -> Result<Principal, TritonError> {
        if let Some(verifier) = &self.oidc {
            // OIDC live → only OIDC. The forwarded-auth fast-path is
            // disabled in this mode so a stale `trust_forwarded_auth=true`
            // env var can never override real PKCE.
            return verify_bearer_via_oidc(verifier, parts).await;
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
        verify_dev_or_reject(token)
    }
}

async fn verify_bearer_via_oidc(
    verifier: &OidcVerifier,
    parts: &Parts,
) -> Result<Principal, TritonError> {
    let token = bearer_from(parts)?;
    verifier.verify(token).await
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
    }
}

#[cfg(feature = "dev-token")]
fn verify_dev_or_reject(token: &str) -> Result<Principal, TritonError> {
    if token != "dev-token" {
        return Err(TritonError::Auth("unknown token".into()));
    }
    Ok(Principal {
        sub: "dev-user".into(),
        scopes: vec!["dev".into()],
        groups: Vec::new(),
        tenant: "dev".into(),
        raw_token: token.into(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    })
}

#[cfg(not(feature = "dev-token"))]
fn verify_dev_or_reject(_token: &str) -> Result<Principal, TritonError> {
    Err(TritonError::Auth(
        "no OIDC verifier configured and dev-token disabled at build time (ADR-10)".into(),
    ))
}
