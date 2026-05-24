//! Tailnet-only `/metrics` listener (FR-O-3, G-5, G-7). Bound on a
//! separate port in `main.rs`; intentionally NOT mounted on the
//! public REST router so Fabio cannot leak it through `:443`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use triton_core::Metrics;

pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(handler))
        .with_state(metrics)
}

async fn handler(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics.render(),
    )
}
