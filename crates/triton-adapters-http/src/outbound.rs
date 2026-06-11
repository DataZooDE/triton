//! Outbound adapter — agent-initiated proactive dispatch (#95).
//!
//! `POST /v1/outbound` lets a registered upstream agent submit a
//! message Triton ships to a known principal on a chat platform,
//! outside any inbound conversation turn. Triton resolves the named
//! adapter's [`OutboundCourier`], which renders the supplied surface
//! through that adapter's surface mapper and posts it via the same
//! private courier the inbound reply leg uses — so credentials,
//! template selection (#94), surface-mapping, and audit stay
//! centralised here rather than leaking into each agent.
//!
//! Per ADR-6 this module is a pure unwrap/wrap shell: identity is
//! delegated to [`crate::identity`]; the dispatcher owns audit
//! emission (the courier impl calls `record_post`). The bearer must
//! carry the DEDICATED outbound audience — a separate
//! [`IdentityProvider`] built from `TRITON_OUTBOUND_AUDIENCE` — so a
//! token minted for the HTTP trio cannot drive proactive sends.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use triton_core::{Dispatcher, OutboundCourier, OutboundRequest};

use crate::identity::IdentityProvider;
use crate::rest::error_response;

/// Registry of adapters that accept agent-initiated sends, keyed by
/// the adapter name from `adapter.yaml` (e.g. `whatsapp`). Built once
/// at boot in `triton-bin` and shared read-only.
pub type CourierRegistry = HashMap<String, Arc<dyn OutboundCourier>>;

#[derive(Clone)]
pub struct OutboundState {
    /// Identity provider gated on the outbound audience. Distinct from
    /// the trio's provider so the audience is the per-surface authZ
    /// boundary.
    pub identity: Arc<IdentityProvider>,
    pub dispatcher: Arc<Dispatcher>,
    pub couriers: Arc<CourierRegistry>,
}

pub fn router(state: OutboundState) -> Router {
    Router::new()
        .route("/v1/outbound", post(outbound_send))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct OutboundBody {
    /// Adapter name from `adapter.yaml` (e.g. `whatsapp`).
    adapter: String,
    /// Platform recipient id (e.g. WhatsApp `wa_id`).
    to: String,
    /// Content to render — the same shape a tool returns. Optional when
    /// a template `category` is supplied (the template body is driven by
    /// `variables` instead).
    #[serde(default)]
    result: serde_json::Value,
    /// Optional template-category hint (#94). When set, the adapter
    /// sends a template rather than free-form text.
    #[serde(default)]
    category: Option<String>,
    /// Ordered template body variables (#94). Only meaningful with
    /// `category`.
    #[serde(default)]
    variables: Vec<String>,
}

async fn outbound_send(
    State(state): State<OutboundState>,
    parts: Parts,
    Json(body): Json<OutboundBody>,
) -> Response {
    // 1. Authn/Authz: bearer must carry the outbound audience.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                "v1/outbound",
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };

    // 2. Resolve the target adapter's courier.
    let Some(courier) = state.couriers.get(&body.adapter) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "not_found",
                "message": format!("no outbound adapter `{}` configured", body.adapter),
            })),
        )
            .into_response();
    };

    // 3. Render + post through the adapter; it emits the `phase: post`
    //    audit sharing `principal.trace_id`.
    let trace_id = principal.trace_id.clone();
    let req = OutboundRequest {
        to: body.to,
        result: body.result,
        category: body.category,
        variables: body.variables,
    };
    match courier.deliver(&req, &principal).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "trace_id": trace_id }))).into_response(),
        Err(e) => error_response(&e, Some(&trace_id)),
    }
}
