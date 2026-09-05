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
/// upstream router can mint a per-call RS256 JWT carrying this
/// principal when dispatching to a static upstream agent.
/// **Never derive `Debug`** — see the manual impl below.
#[derive(Clone, Serialize)]
pub struct Principal {
    pub sub: String,
    pub scopes: Vec<String>,
    /// Group/role memberships of the resolved sender (from the inbound
    /// token's groups/roles claim, or a resolver's identity result). Opt-in
    /// forwarded by the static-upstream router as the NON-authoritative
    /// private claim `triton_sender_groups` — never as `roles` (a downstream
    /// like escurel derives admin from `roles`). Empty by default.
    pub groups: Vec<String>,
    pub tenant: String,
    /// Raw bearer token. Never logged, never audited (FR-AU-3).
    /// Field is `pub` so the upstream router crate can read it; the
    /// redaction discipline lives in the manual `Debug` impl below
    /// and the `serde(skip)` attribute.
    #[serde(skip)]
    pub raw_token: String,
    pub trace_id: String,
    /// The RAW platform sender id this principal was derived from, when
    /// the adapter knows one (`wa_id`, Telegram user id, `users/<id>`,
    /// Teams `from.id`). Recorded in the audit line beside the resolved
    /// subject; never used for authorisation.
    ///
    /// It exists because under `identity.kind: upstream` (FR-I-7) the
    /// resolver REPLACES the asserted identity, so without this the
    /// asserted one is dropped and a session driven by a spoofed sender
    /// id produces audit lines byte-identical to the victim's. On a
    /// boundary that cannot be made cryptographic — the Bot Framework
    /// body is not signed per-user — detection is the compensating
    /// control, and it needs the asserted value on record (#250).
    ///
    /// `None` where there is no platform sender (the HTTP trio, internal
    /// dispatches), in which case the field is omitted from the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_ref: Option<String>,
}

/// Cap on a principal field accepted from an out-of-process identity
/// resolver (FR-I-7 `identity.kind: upstream`).
///
/// Generous: real subjects and tenants are GUIDs, `29:`-prefixed Teams
/// ids, `users/99`, or phone numbers.
pub const MAX_RESOLVED_FIELD_LEN: usize = 128;

/// Validate a `{sub, tenant}` an identity resolver returned, BEFORE the
/// values are used for anything.
///
/// The resolver is out-of-process and its reply is unauthenticated
/// beyond transport, so everything it returns is untrusted input. It has
/// to be checked at the boundary rather than at the point of use,
/// because the points of use are plural and one of them is a map key:
/// `PerTenantBuckets::try_take` inserts `tenant` into a process-lifetime
/// `HashMap`, and that runs before the mint-time check in
/// `static_upstream::bearer`. Its doc-comment promises "the cardinality
/// is bounded by the manifest, not by inbound traffic" — true for
/// `sender_table`, false for `upstream` until this runs first (#250).
///
/// Empty `tenant` is refused here even though an empty tenant claim is a
/// legitimate SHIPPED state: a resolver that answers at all must name a
/// tenant, and the existing resolver code already refuses an empty one.
/// Whitespace and control characters are refused; a stricter allowlist
/// is deliberately not applied — see `static_upstream`'s
/// `validate_signed_field`, which keeps the same rule as defence in
/// depth at mint time.
pub fn validate_resolved(sub: &str, tenant: &str) -> Result<(), crate::error::TritonError> {
    for (field, value) in [("sub", sub), ("tenant", tenant)] {
        if value.trim().is_empty() {
            return Err(crate::error::TritonError::Auth(format!(
                "identity resolver returned an empty {field}"
            )));
        }
        if value.len() > MAX_RESOLVED_FIELD_LEN {
            return Err(crate::error::TritonError::Auth(format!(
                "identity resolver returned a {field} of {} bytes, over the \
                 {MAX_RESOLVED_FIELD_LEN}-byte cap",
                value.len()
            )));
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(crate::error::TritonError::Auth(format!(
                "identity resolver returned a {field} containing whitespace or \
                 control characters"
            )));
        }
    }
    Ok(())
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
            .field("groups", &self.groups)
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
            groups: self.groups.clone(),
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
    pub groups: Vec<String>,
    pub tenant: String,
    pub trace_id: String,
}
