//! REST adapter — well-known operational endpoints (`/healthz`,
//! `/version`) and the tool surface (`POST /v1/tools/:name`, plus
//! the `GET /v1/tools` listing landing in PR 5).
//!
//! Per ADR-6 this module is a pure unwrap/wrap shell: identity is
//! delegated to [`crate::identity`], the dispatcher owns timing,
//! audit emission, **and** the rejected-phase emission for boundary
//! failures (so adapters never own the audit schema). Error
//! variants map to HTTP statuses per architecture.md §8.3.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::ACCEPT;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, extract_surface};
use triton_core::{A2uiVersion, Dispatcher, RuntimeInfo, TritonError, envelope};

use crate::identity::IdentityProvider;

/// Shared state owned by the binary, cloned into every handler via
/// axum `State`. `Arc` everywhere so handler signatures stay cheap
/// and the realization "wrap settings in Arc from the start" holds
/// (Rust port §2).
#[derive(Clone)]
pub struct RestState {
    pub runtime: Arc<RuntimeInfo>,
    pub dispatcher: Arc<Dispatcher>,
    pub identity: Arc<IdentityProvider>,
}

pub fn router(state: RestState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/{name}", post(invoke_tool))
        .with_state(state)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn version(State(state): State<RestState>) -> Json<RuntimeInfo> {
    Json((*state.runtime).clone())
}

/// `GET /v1/tools` — FR-A-5. Authenticated; surfaces every
/// registered tool's name + input JSON schema + `returns_a2ui`
/// flag. Adapters never reach into the registry directly; they
/// ask the dispatcher (ADR-6 — single seam).
async fn list_tools(State(state): State<RestState>, parts: Parts) -> Response {
    // Auth check first; even the listing leaks tool inventory which
    // could be useful to an attacker on a multi-tenant gateway.
    if let Err(e) = state.identity.verify(&parts).await {
        state.dispatcher.record_rejection(
            "v1/tools",
            "rest",
            "-",
            "-",
            &uuid::Uuid::new_v4().to_string(),
            &e,
        );
        return error_response(&e, None);
    }
    Json(json!({ "tools": state.dispatcher.descriptors() })).into_response()
}

async fn invoke_tool(
    State(state): State<RestState>,
    Path(name): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    // Parse the Accept header before any auth check so a malformed
    // A2UI version surfaces as Validation (400) before we even
    // touch identity.
    let requested = match parse_a2ui_accept(&parts) {
        Ok(v) => v,
        Err(e) => {
            state.dispatcher.record_rejection(
                &name,
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };

    // Pre-parse a trace id so a boundary rejection still carries
    // one in the audit line (it'd be misleading to omit). The
    // dispatcher generates its own when a Principal is built; this
    // path is only used when there's no Principal yet.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                &name,
                "rest",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return error_response(&e, None);
        }
    };
    let trace_id = principal.trace_id.clone();
    match state
        .dispatcher
        .invoke_with_bytes(&name, &body, principal, "rest")
        .await
    {
        Ok(mut d) => match wrap_a2ui_if_requested(&mut d, requested) {
            Ok(()) => (StatusCode::OK, Json(envelope(&d))).into_response(),
            Err(e) => error_response(&e, Some(trace_id.as_str())),
        },
        Err(e) => error_response(&e, Some(trace_id.as_str())),
    }
}

/// Wrap a dispatch result into an A2UI envelope when the tool opted
/// in via `returns_a2ui` AND the caller negotiated A2UI. Otherwise
/// the raw result is returned untouched (FR-A-5).
///
/// A tool that advertises `returns_a2ui` but emits an unparseable
/// `surface` is a bug — we surface it as `TritonError::Tool` so the
/// client sees 502 instead of being silently handed raw JSON.
fn wrap_a2ui_if_requested(
    d: &mut triton_core::Dispatch,
    requested: Option<A2uiVersion>,
) -> Result<(), TritonError> {
    let Some(version) = requested else {
        return Ok(());
    };
    if !d.returns_a2ui {
        return Ok(());
    }
    let surface = extract_surface(&d.result).map_err(|e| {
        tracing::warn!(tool_advertised_a2ui = true, error = %e, "tool returned non-A2UI shape");
        TritonError::Tool(format!("tool advertised A2UI but {e}"))
    })?;
    d.result = build_envelope(&surface, version.into());
    Ok(())
}

/// FR-A-3: parse `Accept: application/json+a2ui[; version=0.9]` into
/// an [`A2uiVersion`]. Returns `Some(version)` if **any** Accept
/// range names `application/json+a2ui`, regardless of its position
/// in the comma-separated list. Returns `None` only when no A2UI
/// range is present (caller is happy with plain JSON). Unknown
/// versions inside an A2UI range are an explicit error — never
/// silently downgrade.
fn parse_a2ui_accept(parts: &Parts) -> Result<Option<A2uiVersion>, TritonError> {
    let Some(raw) = parts.headers.get(ACCEPT) else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .map_err(|_| TritonError::Validation("non-ASCII Accept header".into()))?;

    // Walk every comma-separated media range — an A2UI offer
    // anywhere in the list wins over a leading `application/json`
    // (Codex PR 10 finding). We don't implement RFC 9110 q-value
    // sorting; the spec only enumerates two A2UI values.
    let mut found = None;
    for entry in s.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let Some(media) = parts.next() else { continue };
        if media != "application/json+a2ui" {
            continue;
        }
        for param in parts {
            if let Some(version) = param.strip_prefix("version=") {
                let v = match version.trim_matches('"') {
                    "0.8" => A2uiVersion::V08,
                    "0.9" => A2uiVersion::V09,
                    other => {
                        return Err(TritonError::Validation(format!(
                            "unknown A2UI version: {other}"
                        )));
                    }
                };
                return Ok(Some(v));
            }
        }
        found = Some(A2uiVersion::default());
    }
    Ok(found)
}

fn error_response(e: &TritonError, trace_id: Option<&str>) -> Response {
    let status = http_status_for(e);
    let mut body = json!({
        "error": e.class(),
        "message": e.to_string(),
    });
    if let Some(tid) = trace_id {
        body["trace_id"] = json!(tid);
    }
    (status, Json(body)).into_response()
}

fn http_status_for(e: &TritonError) -> StatusCode {
    if e.is_circuit_open() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    if e.is_tool_timeout() {
        return StatusCode::GATEWAY_TIMEOUT;
    }
    match e {
        TritonError::Auth(_) => StatusCode::UNAUTHORIZED,
        TritonError::Validation(_) => StatusCode::BAD_REQUEST,
        TritonError::Tool(_) => StatusCode::BAD_GATEWAY,
        TritonError::Provider(_) => StatusCode::BAD_GATEWAY,
    }
}
