//! Agent-initiated proactive dispatch (#95) — the outbound seam.
//!
//! On the inbound reply leg each chat adapter renders a tool result
//! through its own surface mapper and posts it to the platform via a
//! private courier. The proactive-dispatch API (`POST /v1/outbound`)
//! needs that same render+post path WITHOUT an inbound trigger, so each
//! chat adapter that supports agent-initiated sends implements this
//! trait. The dispatcher stays the single audit pivot (ADR-6): the
//! impl emits its `phase: post` record through
//! [`crate::Dispatcher::record_post`].

use async_trait::async_trait;
use serde_json::Value;

use crate::{Principal, TritonError};

/// One proactive send requested by an upstream agent.
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// Platform recipient id (e.g. WhatsApp `wa_id`, E.164 without the
    /// leading `+`).
    pub to: String,
    /// The content to render — the same shape a tool returns: an A2UI
    /// envelope/surface, or a bare value the adapter's mapper degrades
    /// to text.
    pub result: Value,
    /// Optional template-category hint for platforms that require a
    /// template outside their service window (e.g. WhatsApp Cloud API
    /// utility / marketing / authentication). Wired in #94; ignored by
    /// adapters that don't model a service window.
    pub category: Option<String>,
}

/// A chat adapter that can deliver an agent-initiated outbound message.
///
/// Implementors reuse their existing surface mapper + private courier
/// so credentials, surface-mapping, and audit stay centralised in
/// Triton rather than leaking into upstream agents.
#[async_trait]
pub trait OutboundCourier: Send + Sync {
    /// Audit protocol label, e.g. `messenger:whatsapp`.
    fn protocol(&self) -> &'static str;

    /// Render `req` through the adapter's surface mapper and post it to
    /// the platform, emitting a `phase: post` audit record that shares
    /// `principal.trace_id`.
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError>;
}
