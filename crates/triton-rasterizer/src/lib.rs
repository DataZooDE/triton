//! v0.2 PR 36 — Dashboard rasterizer.
//!
//! Two-faced crate:
//! * **library** (this file + [`svg`] + [`render`]): adapters use
//!   the [`Client`] to POST a Dashboard JSON spec to a running
//!   `triton-rasterizer` sidecar and get back the rendered PNG bytes.
//! * **binary** (`src/bin/triton-rasterizer.rs`): the axum service
//!   that hosts `POST /v1/dashboard.png`.
//!
//! Out-of-process by design (realizations.md §5: "Dashboard
//! rasterisation is out-of-process, not embedded"). Embedding a
//! renderer in the gateway binary would bloat the static link
//! footprint (NFR-PT-2) and bind rasterisation throughput to
//! gateway capacity. Architecture.md §8.7 places the rasterizer
//! squarely on the L6′ degrade boundary for `dashboard:
//! rasterised_png`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use triton_core::a2ui::DashboardTile;

pub mod render;
pub mod svg;

/// Hard ceiling on Dashboard input. DoS guard at the rasterizer
/// edge — refusing oversize payloads at the HTTP boundary keeps
/// the renderer's hot path (SVG parse + raster) bounded.
pub const MAX_TILES: usize = 32;
/// Title byte cap. 256 bytes covers any realistic dashboard label
/// while keeping the SVG template under a few KB.
pub const MAX_TITLE_BYTES: usize = 256;
/// Render-step hard timeout (architecture.md §5.2's "hot path
/// timeouts" pattern). Anything past this earns a 504 from the
/// HTTP layer; the client maps it to [`RasterizerError::Timeout`].
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(2);

/// JSON body the rasterizer accepts. Mirrors
/// `triton_core::a2ui::Component::Dashboard`'s inner shape so the
/// caller can feed the dashboard component through without
/// massaging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardRequest {
    pub title: String,
    pub tiles: Vec<DashboardTile>,
}

impl DashboardRequest {
    /// Validate input caps. Returns a descriptive error string the
    /// HTTP layer can drop straight into a 400 body.
    pub fn validate(&self) -> Result<(), String> {
        if self.title.len() > MAX_TITLE_BYTES {
            return Err(format!(
                "dashboard title exceeds {MAX_TITLE_BYTES}-byte cap (got {} bytes)",
                self.title.len()
            ));
        }
        if self.tiles.len() > MAX_TILES {
            return Err(format!(
                "dashboard has {} tiles; max is {MAX_TILES}",
                self.tiles.len()
            ));
        }
        Ok(())
    }
}

/// Client error categories the chat adapters care about. Each
/// variant lines up with a distinct fallback decision at the
/// adapter edge — e.g. `Timeout` MAY warrant a retry budget, but
/// `BadRequest` is operator-actionable and must surface to logs
/// without retry.
#[derive(Debug, Error)]
pub enum RasterizerError {
    /// Total request exceeded the client's deadline.
    #[error("rasterizer request timed out")]
    Timeout,
    /// Service rejected the input (HTTP 4xx). Body included.
    #[error("rasterizer rejected dashboard: {0}")]
    BadRequest(String),
    /// Service returned 5xx.
    #[error("rasterizer server error: {0}")]
    Server(String),
    /// Could not reach the service (transport, DNS, refused, ...).
    #[error("rasterizer network error: {0}")]
    Network(String),
}

/// Thin HTTP client the chat adapters use to call the rasterizer
/// service. Stateless except for the underlying connection pool.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
}

/// Tunables. Both timeouts default to conservative production
/// values; integration tests override them downward.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL of the rasterizer service, e.g.
    /// `http://127.0.0.1:9320`. Trailing slash optional.
    pub base: String,
    /// Connect timeout — how long we'll wait for the TCP handshake
    /// + initial response headers.
    pub connect_timeout: Duration,
    /// Total request timeout — bounds the full POST round-trip.
    /// Should comfortably exceed [`RENDER_TIMEOUT`] so a render
    /// hitting its server-side cap surfaces as a clean
    /// `Server`/`BadRequest`/timeout-as-network response rather
    /// than racing the client-side cap.
    pub total_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base: "http://127.0.0.1:9320".to_string(),
            connect_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(5),
        }
    }
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self, RasterizerError> {
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.total_timeout)
            .build()
            .map_err(|e| RasterizerError::Network(e.to_string()))?;
        Ok(Self {
            base: cfg.base.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// POST `<base>/v1/dashboard.png` with the dashboard JSON.
    /// Returns PNG bytes on 200; maps everything else to
    /// [`RasterizerError`].
    pub async fn render(&self, req: &DashboardRequest) -> Result<Vec<u8>, RasterizerError> {
        let url = format!("{}/v1/dashboard.png", self.base);
        let resp = self.http.post(&url).json(req).send().await.map_err(|e| {
            if e.is_timeout() {
                RasterizerError::Timeout
            } else {
                RasterizerError::Network(e.to_string())
            }
        })?;
        let status = resp.status();
        if status.is_success() {
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| RasterizerError::Network(e.to_string()))?;
            return Ok(bytes.to_vec());
        }
        let body = resp.text().await.unwrap_or_default();
        if status.is_client_error() {
            // 504 from a downstream rasterizer also shows up here
            // only when the SERVICE itself surfaces a 5xx for the
            // server-side timeout — we keep the categories
            // distinct.
            return Err(RasterizerError::BadRequest(body));
        }
        if status.as_u16() == 504 {
            return Err(RasterizerError::Timeout);
        }
        Err(RasterizerError::Server(format!("{status}: {body}")))
    }
}
