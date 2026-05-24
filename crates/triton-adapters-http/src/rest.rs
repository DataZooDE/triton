//! REST adapter — well-known operational endpoints (`/healthz`,
//! `/version`, `/v1/tools`, `/v1/tools/:name`) and A2UI content
//! negotiation per FR-A-3 / FR-A-5.
//!
//! PR 3 scope: adds `/version` backed by an `Arc<RuntimeInfo>` shared
//! via axum `State` (Rust port realization: wrap settings/state in
//! `Arc` from the start). PR 5 adds the `/v1/tools` surface.

use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};
use triton_core::RuntimeInfo;

/// Build the REST router. The `RuntimeInfo` is shared across handlers
/// via axum state, which requires `Clone`; `Arc` makes that cheap.
pub fn router(runtime: Arc<RuntimeInfo>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .with_state(runtime)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn version(State(runtime): State<Arc<RuntimeInfo>>) -> Json<RuntimeInfo> {
    Json((*runtime).clone())
}
