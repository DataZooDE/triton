//! Per-dispatch delivery context (#352): which channel the turn will be
//! rendered on.
//!
//! Deliberately **not** part of [`crate::Principal`]. The principal is the
//! verified identity: it is serialised into audit lines and minted into the
//! upstream bearer. Where a turn is rendered is presentation metadata. It
//! must never reach a JWT claim or an authorization decision, so it travels
//! beside the identity, not inside it.

/// What the upstream router may tell an agent about where its answer goes.
/// Non-authoritative rendering advice: an agent that ignores it keeps
/// working, and the adapter's surface mapper still degrades the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchContext {
    /// Normalised channel label, e.g. `whatsapp`, `msteams`, `googlechat`,
    /// `rest`, `mcp`, `a2a`. Sent upstream as `X-Triton-Channel`.
    pub channel: String,
}

impl DispatchContext {
    /// Derive the context from the adapter's audit `protocol` label, which
    /// every adapter already passes to the dispatcher.
    pub fn for_protocol(protocol: &str) -> Self {
        Self {
            channel: channel_for_protocol(protocol),
        }
    }
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
