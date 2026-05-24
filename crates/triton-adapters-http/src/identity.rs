//! Identity boundary for the HTTP trio. Holds the optional
//! [`OidcVerifier`] (FR-I-1..3) and the cfg-gated dev-token fallback
//! (ADR-10, FR-I-5). Production builds (`--no-default-features`)
//! reject any non-OIDC bearer at compile time.
//!
//! Verification policy:
//!   * If an OIDC verifier is configured, it is the **only** accepted
//!     identity — even a build with `dev-token` compiled in MUST
//!     reject the dev token when OIDC is live, so an accidental
//!     env-var omission doesn't open a backdoor.
//!   * If no OIDC verifier is configured **and** `dev-token` is
//!     compiled in, the dev token is accepted (local dev only).
//!   * If neither, every bearer is rejected.

use std::sync::Arc;

use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use triton_core::{Principal, TritonError};
use triton_identity::OidcVerifier;

#[derive(Clone)]
pub struct IdentityProvider {
    oidc: Option<Arc<OidcVerifier>>,
}

impl IdentityProvider {
    pub fn new(oidc: Option<Arc<OidcVerifier>>) -> Self {
        Self { oidc }
    }

    pub async fn verify(&self, parts: &Parts) -> Result<Principal, TritonError> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .ok_or_else(|| TritonError::Auth("missing Authorization header".into()))?
            .to_str()
            .map_err(|_| TritonError::Auth("non-ASCII Authorization header".into()))?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| TritonError::Auth("expected `Bearer <token>`".into()))?
            .trim();

        if let Some(verifier) = &self.oidc {
            // OIDC live → only OIDC. Dev token is rejected by the
            // verifier (signature/issuer/audience won't match).
            return verifier.verify(token).await;
        }
        verify_dev_or_reject(token)
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
