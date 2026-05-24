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
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use triton_core::{Dispatcher, RuntimeInfo, TritonError, envelope};

/// Shared state owned by the binary, cloned into every handler via
/// axum `State`. `Arc` everywhere so handler signatures stay cheap
/// and the realization "wrap settings in Arc from the start" holds
/// (Rust port §2).
#[derive(Clone)]
pub struct RestState {
    pub runtime: Arc<RuntimeInfo>,
    pub dispatcher: Arc<Dispatcher>,
}

pub fn router(state: RestState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/v1/tools/{name}", post(invoke_tool))
        .with_state(state)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn version(State(state): State<RestState>) -> Json<RuntimeInfo> {
    Json((*state.runtime).clone())
}

async fn invoke_tool(
    State(state): State<RestState>,
    Path(name): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Response {
    // Pre-parse a trace id so a boundary rejection still carries
    // one in the audit line (it'd be misleading to omit). The
    // dispatcher generates its own when a Principal is built; this
    // path is only used when there's no Principal yet.
    let principal = match crate::identity::principal_from_request(&parts) {
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
    match e {
        TritonError::Auth(_) => StatusCode::UNAUTHORIZED,
        TritonError::Validation(_) => StatusCode::BAD_REQUEST,
        TritonError::Tool(_) => StatusCode::BAD_GATEWAY,
        TritonError::Provider(_) => StatusCode::BAD_GATEWAY,
    }
}
