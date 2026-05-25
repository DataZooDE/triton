//! `triton-rasterizer` — out-of-process dashboard PNG renderer.
//!
//! Runs on a configurable host:port and accepts `POST
//! /v1/dashboard.png` with a [`DashboardRequest`] body. On success
//! returns `Content-Type: image/png`; on bad input returns 400 with
//! a plain-text description; on hot-path timeout returns 504.
//!
//! 12-factor: TRITON_RASTERIZER_HOST + TRITON_RASTERIZER_PORT
//! select the bind. Defaults match the chat adapter's
//! `ClientConfig` default base so a `cargo run --bin
//! triton-rasterizer` in one terminal and a `cargo run --bin
//! triton` in another wire up out-of-the-box.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, serve};
use tokio::net::TcpListener;
use tokio::time::timeout;
use triton_rasterizer::{DashboardRequest, RENDER_TIMEOUT, render, svg};

#[derive(Clone)]
struct AppState {}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();

    let host = std::env::var("TRITON_RASTERIZER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("TRITON_RASTERIZER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9320);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .expect("bind address parses");
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(bind = %addr, "triton-rasterizer: listening");

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/dashboard.png", post(handle_render))
        .with_state(AppState {});

    serve(listener, app).await
}

async fn handle_render(State(_state): State<AppState>, body: Bytes) -> Response {
    // Parse JSON manually (instead of `Json<...>` extractor) so a
    // bad shape produces a 400 with our message rather than the
    // axum default. The error string MUST NOT include the body
    // (could contain sensitive numbers — never log full tile
    // contents at info level, per the spec).
    let req: DashboardRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("malformed dashboard body: {e}"),
            )
                .into_response();
        }
    };
    if let Err(msg) = req.validate() {
        // Validation rejection includes the cap that was breached,
        // not the body — safe to surface.
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    // Title + tile-count is the only thing we ever log at info
    // level (spec constraint: tile contents may be sensitive).
    tracing::info!(
        title = %req.title,
        tile_count = req.tiles.len(),
        "rendering dashboard",
    );

    // Build + render inside the timeout. `spawn_blocking` isolates
    // the CPU-bound raster step from the runtime's reactor; a
    // pathological SVG can't starve other requests beyond the
    // server's worker pool size.
    let render_result = timeout(RENDER_TIMEOUT, async move {
        tokio::task::spawn_blocking(move || {
            let (svg_doc, height) = svg::build(&req);
            render::render_png(&svg_doc, 1200, height)
        })
        .await
    })
    .await;

    let png = match render_result {
        Err(_elapsed) => {
            tracing::warn!("rasterizer hot-path timeout ({:?})", RENDER_TIMEOUT);
            return (StatusCode::GATEWAY_TIMEOUT, "render timeout").into_response();
        }
        Ok(Err(join_err)) => {
            tracing::error!(error = %join_err, "rasterizer worker panicked");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
        Ok(Ok(Err(render_err))) => {
            tracing::warn!(error = %render_err, "rasterizer render failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("render: {render_err}"),
            )
                .into_response();
        }
        Ok(Ok(Ok(bytes))) => bytes,
    };

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("image/png"));
    (StatusCode::OK, headers, png).into_response()
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .json()
        .init();
}
