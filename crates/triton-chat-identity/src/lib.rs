//! FR-I-7 identity resolution, owned once (#289).
//!
//! `SenderClaims`, the `IdentityMode` enum, the boot-time `IdentityKind`
//! match and `resolve_via_upstream` had been copied into eight chat
//! adapters. The cost was never the duplication itself — it was what
//! duplication does to an invariant. A rule added to this seam has to be
//! added eight times, and the eighth is skipped by *omission* rather
//! than by decision. `validate_resolved` is the worked example: it
//! guards the `upstream` path in three adapters, and no path at all in
//! the rest — including the `sender_table` path in every one of them,
//! where the same values reach the same places (`PerTenantBuckets`
//! makes `tenant` a process-lifetime map key; `static_upstream::bearer`
//! signs it into a token).
//!
//! So the rules live here, and they are not opt-in:
//!
//! * [`SenderTable::parse`] validates every entry at BOOT. A table an
//!   operator cannot use safely refuses the deploy that carries it
//!   rather than the first message after it.
//! * [`UpstreamResolver::resolve`] validates the reply before returning
//!   it. There is no way to obtain a [`Resolved`] that skipped the
//!   check, because the type is only constructed here.
//!
//! What is deliberately NOT here: `azure` (MS Teams' Entra config) and
//! `self_enrol` (Google Chat's pairing table). Those genuinely differ
//! per adapter today, and CLAUDE.md §4 says to extract a trait when the
//! fourth concrete case appears, not in anticipation of it. They stay
//! adapter-owned until they stop differing.

use std::collections::HashMap;

use serde::Deserialize;
use triton_core::dispatcher::Dispatcher;
use triton_core::error::TritonError;
use triton_core::principal::{Principal, validate_resolved};
use triton_manifest::IdentityKind;

/// One entry of an operator-authored `identity.table`: the claims a
/// platform sender id maps to.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Present on the adapters that forward RBAC groups; absent
    /// elsewhere, where it stays empty rather than becoming an error.
    #[serde(default)]
    pub groups: Vec<String>,
    pub tenant: String,
}

/// What identity resolution produced. Only this crate constructs one,
/// which is what makes the validation unskippable: an adapter cannot
/// hold a `Resolved` that was never checked.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub sub: String,
    pub scopes: Vec<String>,
    pub groups: Vec<String>,
    pub tenant: String,
}

impl From<&SenderClaims> for Resolved {
    fn from(c: &SenderClaims) -> Self {
        Self {
            sub: c.sub.clone(),
            scopes: c.scopes.clone(),
            groups: c.groups.clone(),
            tenant: c.tenant.clone(),
        }
    }
}

/// A validated `identity.table`.
///
/// The newtype is the point: a bare `HashMap<String, SenderClaims>` can
/// be built from unvalidated JSON anywhere, and eight adapters proved
/// that it would be.
#[derive(Debug, Clone, Default)]
pub struct SenderTable(HashMap<String, SenderClaims>);

impl SenderTable {
    /// Parse and validate an `identity.table` JSON document.
    ///
    /// Every entry's `sub` and `tenant` go through the same
    /// [`validate_resolved`] the `upstream` path uses. The table is
    /// operator-authored and so lower-risk than a resolver reply, but
    /// "lower-risk" is not the same as "checked", and the values end up
    /// in exactly the same places. Refusing at boot also turns a class
    /// of silent misconfiguration into a failed deploy, which is the
    /// only moment an operator is looking.
    pub fn parse(json: &str) -> Result<Self, IdentityError> {
        let raw: HashMap<String, SenderClaims> =
            serde_json::from_str(json).map_err(|e| IdentityError::TableParse(e.to_string()))?;
        for (key, claims) in &raw {
            validate_resolved(&claims.sub, &claims.tenant).map_err(|e| {
                IdentityError::TableEntry {
                    // The KEY, not the claims: an operator fixing this
                    // is looking at a JSON object and needs to know
                    // which entry, and the sub may itself be the
                    // unprintable thing that failed.
                    key: key.clone(),
                    reason: e.to_string(),
                }
            })?;
        }
        Ok(Self(raw))
    }

    pub fn get(&self, sender_key: &str) -> Option<&SenderClaims> {
        self.0.get(sender_key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Resolve a sender, or `None` when the table does not name them.
    /// `None` is an auth refusal at every call site; it is returned
    /// rather than an error so the adapter can audit it in its own
    /// shape.
    pub fn resolve(&self, sender_key: &str) -> Option<Resolved> {
        self.get(sender_key).map(Resolved::from)
    }
}

/// The `upstream` strategy: resolution delegated to a tool reached
/// through the upstream router.
#[derive(Debug, Clone)]
pub struct UpstreamResolver {
    tool: String,
    /// The `platform` value sent to the resolver (`"telegram"`, …).
    platform: String,
    /// The audit protocol label for the resolver dispatch. Distinct
    /// from the adapter's own so a resolve call's audit lines never
    /// blur with the real command's.
    protocol: String,
}

impl UpstreamResolver {
    /// Build a resolver, refusing the two configurations that fail
    /// silently at runtime.
    ///
    /// A resolver tool whose name collides with an IN-PROCESS tool is
    /// the dangerous one: `Dispatcher::invoke` would run the local tool
    /// and quietly bypass both the router and the per-call upstream
    /// token, so identity would be decided by whatever in-process tool
    /// happened to share the name.
    pub fn new(
        tool: impl Into<String>,
        platform: impl Into<String>,
        protocol: impl Into<String>,
        dispatcher: &Dispatcher,
    ) -> Result<Self, IdentityError> {
        let tool = tool.into();
        if tool.trim().is_empty() {
            return Err(IdentityError::EmptyResolverTool);
        }
        if dispatcher.descriptors().iter().any(|d| d.name == tool) {
            return Err(IdentityError::ResolverToolCollision(tool));
        }
        Ok(Self {
            tool,
            platform: platform.into(),
            protocol: protocol.into(),
        })
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Say the trust model out loud, once, at boot.
    ///
    /// `upstream` resolution is an authorization-table LOOKUP, not a
    /// verification: the resolver is keyed on the platform sender id,
    /// which arrives in the request body and is signed by nothing.
    /// Whoever can cause the platform to deliver a message bearing a
    /// chosen sender id inherits that sender's principal. That is
    /// acceptable for the deployments this mode was built for, but it
    /// should be a decision an operator sees rather than one buried in
    /// a doc comment.
    pub fn warn_trust_model(&self, adapter_name: &str) {
        tracing::warn!(
            adapter = %adapter_name,
            resolver_tool = %self.tool,
            "identity.kind `upstream`: the resolver maps an UNSIGNED platform sender id to a \
             principal. It is an authorization table, not a cryptographic identity proof — \
             anyone able to present a chosen sender id inherits that sender's tenant and \
             scopes. See doc/realizations.md §7."
        );
    }

    /// Resolve a sender through the resolver tool.
    ///
    /// The reply is validated before it is returned — at the BOUNDARY,
    /// not at the point of use, because the points of use are plural
    /// and one of them is a map key. `PerTenantBuckets::try_take`
    /// inserts `tenant` into a process-lifetime `HashMap` well before
    /// the mint-time check in `static_upstream::bearer` runs (#250).
    pub async fn resolve(
        &self,
        dispatcher: &Dispatcher,
        sender_key: &str,
    ) -> Result<Resolved, TritonError> {
        if sender_key.is_empty() {
            return Err(TritonError::Auth(
                "empty sender for upstream resolver".into(),
            ));
        }
        let bootstrap = Principal {
            sub: "identity-resolver".to_string(),
            scopes: vec!["resolve".to_string()],
            groups: Vec::new(),
            tenant: "system".to_string(),
            raw_token: String::new(),
            trace_id: uuid::Uuid::new_v4().to_string(),
            // #250: this principal exists only on the `upstream` path,
            // and the sender it names is the whole subject of the call
            // — so the resolver dispatch is auditable against the id it
            // was asked about, not only against the answer it gave.
            sender_ref: Some(sender_key.to_string()),
        };
        let args = serde_json::json!({ "platform": self.platform, "sender": sender_key });
        let dispatch = dispatcher
            .invoke(&self.tool, args, bootstrap, &self.protocol)
            .await
            .map_err(|e| TritonError::Auth(format!("identity resolver `{}`: {e}", self.tool)))?;
        let reply: ResolverReply = serde_json::from_value(dispatch.result).map_err(|e| {
            TritonError::Auth(format!("resolver reply not {{sub,scopes,tenant}}: {e}"))
        })?;
        validate_resolved(&reply.sub, &reply.tenant)?;
        Ok(Resolved {
            sub: reply.sub,
            scopes: reply.scopes,
            groups: reply.groups,
            tenant: reply.tenant,
        })
    }
}

/// The shape an `upstream` resolver tool must reply with.
#[derive(Debug, Deserialize)]
struct ResolverReply {
    sub: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
    tenant: String,
}

/// Refuse an `identity.kind` the adapter does not implement.
///
/// This replaces the ad-hoc `other => return Err(...)` arm each adapter
/// carried, and more importantly the one an adapter could forget: a
/// missing arm means a kind is accepted and then silently behaves like
/// whatever the `match` fell through to.
pub fn require_supported_kind(
    adapter_name: &str,
    kind: &IdentityKind,
    supported: &[IdentityKind],
) -> Result<(), IdentityError> {
    if supported.contains(kind) {
        return Ok(());
    }
    Err(IdentityError::UnsupportedKind {
        adapter: adapter_name.to_string(),
        got: format!("{kind:?}"),
        supported: supported
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", "),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
    #[error("identity.table entry `{key}` is unusable: {reason}")]
    TableEntry { key: String, reason: String },
    #[error("identity.resolver_tool must be non-empty")]
    EmptyResolverTool,
    #[error(
        "identity.resolver_tool `{0}` collides with an in-process tool; the upstream resolver \
         must be a distinct upstream agent, or identity would be decided in-process without \
         the router or the per-call upstream token"
    )]
    ResolverToolCollision(String),
    #[error("{adapter} adapter supports `identity.kind`: {supported}; got {got}")]
    UnsupportedKind {
        adapter: String,
        got: String,
        supported: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_table_parses() {
        let t = SenderTable::parse(
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                "43":{"sub":"29:1abc","scopes":[],"tenant":"28c0071d-815c-4ace-a3b5-9a28bde005fd"}}"#,
        )
        .expect("parses");
        assert_eq!(t.len(), 2);
        let r = t.resolve("42").expect("alice is in the table");
        assert_eq!(r.sub, "alice");
        assert_eq!(r.tenant, "acme");
        assert!(t.resolve("99").is_none());
    }

    #[test]
    fn groups_default_to_empty_rather_than_failing() {
        // Most adapters' tables have never carried `groups`. Making it
        // required would break every one of them at boot.
        let t = SenderTable::parse(r#"{"42":{"sub":"a","tenant":"acme"}}"#).expect("parses");
        assert!(t.resolve("42").unwrap().groups.is_empty());
    }

    #[test]
    fn an_unusable_entry_refuses_the_whole_table() {
        for bad in [
            r#"{"42":{"sub":"alice","tenant":"ac me"}}"#,
            r#"{"42":{"sub":"al\nice","tenant":"acme"}}"#,
            r#"{"42":{"sub":"","tenant":"acme"}}"#,
            r#"{"42":{"sub":"alice","tenant":""}}"#,
        ] {
            assert!(
                SenderTable::parse(bad).is_err(),
                "must refuse the table: {bad}"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_entry_not_the_value() {
        // The `sub` may itself be the unprintable thing that failed, so
        // an operator needs the KEY to find it in their JSON.
        let e = SenderTable::parse(r#"{"tg-42":{"sub":"al\nice","tenant":"acme"}}"#)
            .expect_err("refused");
        assert!(format!("{e}").contains("tg-42"), "{e}");
    }

    #[test]
    fn one_bad_entry_condemns_the_table() {
        // Not "skip the bad entry and carry on": that would silently
        // lock out one user, and the operator would debug it as a
        // platform problem.
        assert!(
            SenderTable::parse(
                r#"{"42":{"sub":"alice","tenant":"acme"},
                    "43":{"sub":"bob","tenant":"glo bex"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn an_unsupported_kind_names_what_is_supported() {
        let e = require_supported_kind(
            "telegram",
            &IdentityKind::Azure,
            &[IdentityKind::SenderTable, IdentityKind::Upstream],
        )
        .expect_err("azure is not supported by telegram");
        let msg = format!("{e}");
        assert!(msg.contains("telegram"), "{msg}");
        assert!(msg.contains("Azure"), "{msg}");
        assert!(msg.contains("SenderTable"), "{msg}");
    }

    #[test]
    fn a_supported_kind_passes() {
        assert!(
            require_supported_kind(
                "telegram",
                &IdentityKind::Upstream,
                &[IdentityKind::SenderTable, IdentityKind::Upstream],
            )
            .is_ok()
        );
    }
}
