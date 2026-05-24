//! Identity boundary for the HTTP trio. PR 4 ships the dev-token
//! path only (ADR-10, FR-I-5); PR 8 adds the OIDC verifier and turns
//! the dev-token path into a compile-time-gated fallback. Production
//! builds (`--no-default-features`) reject any non-OIDC bearer at
//! compile time.

use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use triton_core::{Principal, TritonError};

/// Build a [`Principal`] from the inbound request's `Authorization`
/// header. Returns `Auth` for missing/malformed bearer; later PRs
/// will surface `Auth` for invalid OIDC signatures too.
pub fn principal_from_request(parts: &Parts) -> Result<Principal, TritonError> {
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

    verify_token(token)
}

#[cfg(feature = "dev-token")]
fn verify_token(token: &str) -> Result<Principal, TritonError> {
    if token != "dev-token" {
        return Err(TritonError::Auth("unknown token".into()));
    }
    Ok(Principal {
        sub: "dev-user".into(),
        scopes: vec!["dev".into()],
        tenant: "dev".into(),
        // Never logged or audited — the raw token sticks around
        // only so the upstream router (PR 9) can mint a Vault-swap
        // OIDC token from it.
        raw_token: token.into(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    })
}

#[cfg(not(feature = "dev-token"))]
fn verify_token(_token: &str) -> Result<Principal, TritonError> {
    Err(TritonError::Auth(
        "OIDC verifier not yet wired; rebuild with `--features dev-token` or wait for PR 8".into(),
    ))
}
