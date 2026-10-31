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
    /// The chat conversation this turn arrived on, as a channel-specific
    /// reference an out-of-band send can later reach (Teams: the flattened
    /// `conversationReference`). A chat adapter that can capture one sets it; it
    /// rides the (in-process, embedded) dispatch into the upstream agent's turn
    /// so a tool like `start_operation` can persist it and deliver a result back
    /// to this conversation later. `None` off-chat or where no reference exists.
    /// (A remote upstream would additionally need this forwarded as a header;
    /// today it serves the in-process embedded host.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_ref: Option<serde_json::Value>,
    /// The external identity provider that vouched for this caller, when
    /// the inbound path verified one: which provider (`google`,
    /// `entra`, `oidc`, `msteams`), the provider's own tenant (Google
    /// `hd`, Entra `tid`) and the verified email. `None` on paths without
    /// a provider (dev token, forwarded auth, sender tables). Forwarded to
    /// a static upstream as the `idp`, `idp_tenant` and `email` claims only
    /// with `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idp: Option<IdpIdentity>,
}

/// What an external identity provider asserted about a caller. Every field
/// comes from a verified token (or a provider's own introspection), never
/// from a request body or header.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct IdpIdentity {
    /// `google`, `entra`, `oidc` (any other configured issuer) or
    /// `msteams` (the Bot Framework channel identity).
    pub name: String,
    /// The provider's tenant: Google Workspace `hd`, Entra `tid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The verified email (lower-cased), or the Entra sign-in name when
    /// the token carries no `email` claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The verified issuer that vouched for the caller (the token's `iss`,
    /// `https://accounts.google.com`, `https://api.botframework.com`).
    /// Distinguishes two configured issuers that share a provider name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

impl IdpIdentity {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tenant: None,
            email: None,
            issuer: None,
        }
    }

    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    pub fn tenant(mut self, tenant: Option<String>) -> Self {
        self.tenant = tenant.filter(|t| !t.trim().is_empty());
        self
    }

    pub fn email(mut self, email: Option<String>) -> Self {
        self.email = email
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        self
    }
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
/// Tenants that are NOT a tenant: shared markers many unrelated callers
/// carry at once.
///
/// `-` is what a single-tenant OIDC caller with no `tenant` claim gets,
/// and what every opaque Google access-token caller gets — so on the live
/// deployment it is nearly every human. `pairing` is the bucket every
/// un-enrolled chat sender shares. An empty tenant is the same thing
/// spelled differently.
///
/// They matter because tenant-scoping is an EQUALITY test. Two callers
/// carrying `-` are not in the same tenant; they are both unattributed,
/// and treating that as a match hands one caller the other's rows. Any
/// scoping decision must therefore refuse to match on these rather than
/// comparing them.
pub const RESERVED_TENANTS: &[&str] = &["-", "pairing", ""];

/// True when `tenant` is a shared marker rather than a real tenant, and
/// so must never satisfy a scoping equality check.
pub fn is_reserved_tenant(tenant: &str) -> bool {
    RESERVED_TENANTS.contains(&tenant.trim())
}

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
            .field("idp", &self.idp)
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
            conversation_ref: self.conversation_ref.clone(),
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
    /// The turn's chat conversation reference (see [`Principal::conversation_ref`]),
    /// so an in-process tool can attach it to durable work it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_ref: Option<serde_json::Value>,
}
