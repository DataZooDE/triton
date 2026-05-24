//! REST adapter — well-known operational endpoints (`/healthz`,
//! `/version`, `/v1/tools`, `/v1/tools/:name`) and A2UI content
//! negotiation per FR-A-3 / FR-A-5.
//!
//! PR 1: only `/healthz`. The router shape is intentionally kept thin
//! so further endpoints land as additional `.route(...)` calls without
//! restructuring.

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};

/// Build the REST router. No state for the walking skeleton; later
/// PRs will accept `Arc<Settings>` and `Arc<ToolRegistry>` as
/// dependencies (FR-A-2, ADR-6).
pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
