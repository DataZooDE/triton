//! The verified identity carried through the dispatcher into the
//! upstream router (FR-I-4). Adapters construct a `Principal` after
//! the platform-appropriate identity check (OIDC for the HTTP trio,
//! per-platform signature schemes for chat-channel adapters); the
//! dispatcher treats them all identically.
//!
//! Two types intentionally — `Principal` is the full credentialed
//! identity the dispatcher and upstream router hold, and
//! [`ToolPrincipal`] is the redacted view tools see. Splitting
//! makes the lethal-trifecta cut (§2 invariant 7) explicit in the
//! type system: an in-process tool can never accidentally exfiltrate
//! the inbound bearer token because it cannot see it.

use std::fmt;

use serde::Serialize;

/// Dispatcher-internal identity. Holds the raw bearer so the
/// upstream router (PR 9) can mint a Vault-swap OIDC token from it.
/// **Never derive `Debug`** — see the manual impl below.
#[derive(Clone, Serialize)]
pub struct Principal {
    pub sub: String,
    pub scopes: Vec<String>,
    pub tenant: String,
    /// Raw bearer token. Never logged, never audited (FR-AU-3).
    /// Field is `pub` so the upstream router crate can read it; the
    /// redaction discipline lives in the manual `Debug` impl below
    /// and the `serde(skip)` attribute.
    #[serde(skip)]
    pub raw_token: String,
    pub trace_id: String,
}

/// `Debug` is manual so an accidental `tracing!(?principal)` or
/// panic message never prints the raw token. The redacted form
/// shows everything operators need to triage without ever revealing
/// `raw_token`.
impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Principal")
            .field("sub", &self.sub)
            .field("scopes", &self.scopes)
            .field("tenant", &self.tenant)
            .field("raw_token", &"<redacted>")
            .field("trace_id", &self.trace_id)
            .finish()
    }
}

impl Principal {
    /// Redacted view passed to [`Tool::invoke`]; tools never see the
    /// raw bearer.
    pub fn to_tool_principal(&self) -> ToolPrincipal {
        ToolPrincipal {
            sub: self.sub.clone(),
            scopes: self.scopes.clone(),
            tenant: self.tenant.clone(),
            trace_id: self.trace_id.clone(),
        }
    }
}

/// What [`Tool::invoke`] (and any in-process handler) sees — a copy
/// of [`Principal`] minus `raw_token`. By construction, an
/// in-process tool cannot read the inbound bearer.
#[derive(Debug, Clone, Serialize)]
pub struct ToolPrincipal {
    pub sub: String,
    pub scopes: Vec<String>,
    pub tenant: String,
    pub trace_id: String,
}
