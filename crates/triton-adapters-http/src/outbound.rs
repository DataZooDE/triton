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
use triton_core::ratelimit::{PerTenantBuckets, TokenBucket};
use triton_core::{Dispatcher, OutboundCourier, OutboundRequest, Principal, TritonError};

use crate::identity::IdentityProvider;
use crate::rest::error_response;

/// Capability scope a token MUST carry to use `/v1/outbound` (#113).
/// The dedicated outbound audience says "this token is for the outbound
/// surface"; this scope says "...and it may send". Both are required.
const REQUIRED_OUTBOUND_SCOPE: &str = "outbound:send";

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
    /// #115: adapter-wide outbound DoS floor (global token bucket).
    pub rate_limit: Arc<TokenBucket>,
    /// #115: per-tenant fair-share, keyed by the caller's tenant.
    pub per_tenant_limit: Arc<PerTenantBuckets>,
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

    // 2. Authz layer 1 (#113): the token must carry the outbound
    //    capability scope, not just the audience.
    if !principal
        .scopes
        .iter()
        .any(|s| s == REQUIRED_OUTBOUND_SCOPE)
    {
        let e = TritonError::Forbidden(format!(
            "outbound send requires the `{REQUIRED_OUTBOUND_SCOPE}` scope"
        ));
        state.dispatcher.record_rejection(
            "v1/outbound",
            "rest",
            &principal.sub,
            &principal.tenant,
            &principal.trace_id,
            &e,
        );
        return error_response(&e, Some(&principal.trace_id));
    }

    // 3. Rate limit (#115): global floor, then per-tenant fair share —
    //    consumed before courier work so a throttled send is cheap.
    if let Err(retry) = state.rate_limit.try_take() {
        return rate_limited(&state, &principal, retry);
    }
    if let Err(retry) = state.per_tenant_limit.try_take(&principal.tenant) {
        return rate_limited(&state, &principal, retry);
    }

    // 4. Resolve the target adapter's courier.
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

    let trace_id = principal.trace_id.clone();
    let req = OutboundRequest {
        to: body.to,
        result: body.result,
        category: body.category,
        variables: body.variables,
    };

    // 5. Authz layer 2 (#113): recipient/tenant binding, behind the
    //    adapter (it knows its identity model). Runs before delivery.
    if let Err(e) = courier.authorize(&req, &principal).await {
        state.dispatcher.record_rejection(
            "v1/outbound",
            "rest",
            &principal.sub,
            &principal.tenant,
            &trace_id,
            &e,
        );
        return error_response(&e, Some(&trace_id));
    }

    // 6. Render + post through the adapter; it emits the `phase: post`
    //    audit sharing `principal.trace_id`.
    match courier.deliver(&req, &principal).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "trace_id": trace_id }))).into_response(),
        Err(e) => error_response(&e, Some(&trace_id)),
    }
}

/// Build a `429` for a throttled outbound (#115): audit the rejection and
/// return a `Retry-After` header, mirroring the chat adapters' shape.
fn rate_limited(state: &OutboundState, principal: &Principal, retry_after: f64) -> Response {
    let secs = retry_after.ceil().max(1.0) as u64;
    let e = TritonError::RateLimited(format!(
        "outbound rate limit for tenant `{}`; retry in {secs}s",
        principal.tenant
    ));
    state.dispatcher.record_rejection(
        "v1/outbound",
        "rest",
        &principal.sub,
        &principal.tenant,
        &principal.trace_id,
        &e,
    );
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("Retry-After", secs.to_string())],
        Json(json!({ "error": "ratelimit", "message": e.to_string() })),
    )
        .into_response()
}
