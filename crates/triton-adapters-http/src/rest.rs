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
    // touch identity. The parsed version isn't actually used by
    // the dispatcher yet — PR 10 wires the envelope-builder.
    let _requested = match parse_a2ui_accept(&parts) {
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
        Ok(d) => (StatusCode::OK, Json(envelope(&d))).into_response(),
        Err(e) => error_response(&e, Some(trace_id.as_str())),
    }
}

/// FR-A-3: parse `Accept: application/json+a2ui[; version=0.9]` into
/// an [`A2uiVersion`]. Returns `None` (caller treats as "plain JSON,
/// no A2UI envelope wanted") when the header is absent or asks for
/// `application/json`. Returns `Some(version)` for the documented
/// A2UI media types. Returns `Validation` for any other concrete
/// A2UI request — unknown versions are an explicit error so the
/// caller learns about the drift instead of silently downgrading.
fn parse_a2ui_accept(parts: &Parts) -> Result<Option<A2uiVersion>, TritonError> {
    let Some(raw) = parts.headers.get(ACCEPT) else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .map_err(|_| TritonError::Validation("non-ASCII Accept header".into()))?;

    // Find the first media-range matching application/json+a2ui or
    // application/json. We don't implement RFC 9110 q-value sorting
    // — Triton sees one caller at a time and the spec only enumerates
    // the two documented values.
    for entry in s.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let Some(media) = parts.next() else { continue };
        match media {
            "application/json+a2ui" => {
                for param in parts {
                    if let Some(version) = param.strip_prefix("version=") {
                        return match version.trim_matches('"') {
                            "0.8" => Ok(Some(A2uiVersion::V08)),
                            "0.9" => Ok(Some(A2uiVersion::V09)),
                            other => Err(TritonError::Validation(format!(
                                "unknown A2UI version: {other}"
                            ))),
                        };
                    }
                }
                return Ok(Some(A2uiVersion::default()));
            }
            // No A2UI envelope requested; plain JSON response.
            "application/json" | "*/*" => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
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
