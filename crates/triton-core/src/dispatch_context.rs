//! Per-dispatch delivery context (#352): which channel the turn will be
//! rendered on.
//!
//! Deliberately **not** part of [`crate::Principal`]. The principal is the
//! verified identity: it is serialised into audit lines and minted into the
//! upstream bearer. Where a turn is rendered is presentation metadata. It
//! must never reach a JWT claim or an authorization decision, so it travels
//! beside the identity, not inside it.

use std::sync::Arc;

use crate::surface::{SurfaceDescriptor, SurfaceRegistry};

/// What the upstream router may tell an agent about where its answer goes.
/// Non-authoritative rendering advice: an agent that ignores it keeps
/// working, and the adapter's surface mapper still degrades the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchContext {
    /// Normalised channel label, e.g. `whatsapp`, `msteams`, `googlechat`,
    /// `rest`, `mcp`, `a2a`. Sent upstream as `X-Triton-Channel`.
    pub channel: String,
    /// What the channel can show, sent as `_triton.surface`. `None` for a
    /// channel with no registered profile.
    pub surface: Option<Arc<SurfaceDescriptor>>,
}

impl DispatchContext {
    /// Derive the context from the adapter's audit `protocol` label, which
    /// every adapter already passes to the dispatcher, and look the channel
    /// up in `surfaces`.
    pub fn for_protocol(protocol: &str, surfaces: &SurfaceRegistry) -> Self {
        let channel = channel_for_protocol(protocol);
        let surface = surfaces.get(&channel);
        Self { channel, surface }
    }
}

/// What an adapter adds to the context the dispatcher derives on its own
/// (#352). Spec A2A uses it: its surface depends on the extensions the
/// client activated on *this* request, and it carries per-turn metadata.
#[derive(Debug, Clone, Default)]
pub struct ContextOverrides {
    /// Written into object args under `_triton` (e.g. `protocol`,
    /// `a2a_context_id`, `agent`), for in-process tools and remote agents
    /// alike. Replaces whatever a caller put under that key.
    pub triton_meta: serde_json::Map<String, serde_json::Value>,
    /// A request-scoped surface, in place of the channel's registered one.
    pub surface: Option<Arc<SurfaceDescriptor>>,
    /// The client's files of this turn (spec A2A), written into args as
    /// `_triton.client.files` at dispatch. Kept apart from `triton_meta`,
    /// which a task stores: a resumed task runs without them.
    pub client_files: Option<Arc<Vec<serde_json::Value>>>,
}

/// Write the channel's surface descriptor into object args under
/// `_triton.surface` (#352), exactly as the static upstream router sends it
/// to an HTTP agent. An in-process [`crate::UpstreamDispatch`] that hands
/// the args to an agent loop calls this so the agent sees the same body
/// over either transport. Non-object args, and a context without a
/// registered surface, are returned unchanged.
pub fn surface_hint(mut args: serde_json::Value, ctx: &DispatchContext) -> serde_json::Value {
    let (Some(surface), Some(map)) = (&ctx.surface, args.as_object_mut()) else {
        return args;
    };
    let reserved = map
        .entry(crate::dispatcher::RESERVED_ARGS_KEY)
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let Some(reserved) = reserved.as_object_mut() {
        reserved.insert(
            "surface".to_string(),
            serde_json::to_value(surface.as_ref()).unwrap_or(serde_json::Value::Null),
        );
    }
    args
}

/// Map an adapter's audit `protocol` label to the channel name agents see.
///
/// * `messenger:<adapter>` and `messenger:<adapter>:<role>` (e.g. the
///   identity resolver, `messenger:telegram:identity`) → `<adapter>`: the
///   channel is where the user is, whatever the dispatch is for.
/// * `google_chat` → `googlechat`, the name used everywhere else (surface
///   preview, docs) for the same channel.
/// * The HTTP trio (`rest`, `mcp`, `a2a`) passes through unchanged.
pub fn channel_for_protocol(protocol: &str) -> String {
    let adapter = match protocol.strip_prefix("messenger:") {
        Some(rest) => rest.split(':').next().unwrap_or(rest),
        None => protocol,
    };
    match adapter {
        "google_chat" => "googlechat".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx(channel: &str) -> DispatchContext {
        DispatchContext::for_protocol(channel, &SurfaceRegistry::with_trio_defaults())
    }

    /// An in-process upstream sees the same body an HTTP agent receives:
    /// the channel's descriptor under `_triton.surface`, beside whatever
    /// else Triton put under `_triton`.
    #[test]
    fn surface_hint_writes_the_descriptor_under_the_reserved_key() {
        let ctx = ctx("mcp");
        let args = json!({ "message": "hi", "_triton": { "protocol": "mcp" } });
        let out = surface_hint(args, &ctx);
        assert_eq!(out["message"], "hi");
        assert_eq!(out["_triton"]["protocol"], "mcp");
        assert_eq!(
            out["_triton"]["surface"],
            serde_json::to_value(ctx.surface.as_deref().expect("mcp has a profile")).unwrap()
        );
    }

    #[test]
    fn surface_hint_leaves_non_objects_and_unknown_channels_alone() {
        assert_eq!(surface_hint(json!("text"), &ctx("mcp")), json!("text"));
        let none = ctx("no-such-channel");
        assert!(none.surface.is_none());
        assert_eq!(surface_hint(json!({ "a": 1 }), &none), json!({ "a": 1 }));
    }
}
