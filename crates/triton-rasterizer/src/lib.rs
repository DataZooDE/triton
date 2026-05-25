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
/// Per-tile string byte cap. PR 38 (codex review): without this an
/// oversize `label` / `value` / `trend` would balloon the SVG
/// template even though `MAX_TILES` / `MAX_TITLE_BYTES` look healthy.
/// 128 bytes covers any reasonable tile label or value while keeping
/// the worst-case SVG body (32 tiles × 3 strings × ~5x XML-escape
/// blowup) under ~80 KB — safe for `usvg` to parse + `tiny-skia` to
/// raster within the [`RENDER_TIMEOUT`] budget.
pub const MAX_TILE_FIELD_BYTES: usize = 128;
/// Client-side response body cap (PR 38 codex review). A
/// well-behaved rasterizer never produces a >2 MiB PNG for the 1200-
/// wide canvas; capping defends the adapter hot path from a
/// misbehaving or attacker-controlled rasterizer flooding memory
/// before the chat-platform courier even runs.
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
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
        // PR 38 (codex review): per-tile field cap. Mirrors the
        // title/tile-count caps above so an oversize tile string
        // is rejected at the HTTP boundary, not mid-render after
        // we've allocated the SVG buffer. The error string names
        // the offending tile index + field so the operator can
        // pinpoint the misbehaving tool.
        for (i, tile) in self.tiles.iter().enumerate() {
            if tile.label.len() > MAX_TILE_FIELD_BYTES {
                return Err(format!(
                    "tile[{i}].label exceeds {MAX_TILE_FIELD_BYTES}-byte cap (got {} bytes)",
                    tile.label.len()
                ));
            }
            if tile.value.len() > MAX_TILE_FIELD_BYTES {
                return Err(format!(
                    "tile[{i}].value exceeds {MAX_TILE_FIELD_BYTES}-byte cap (got {} bytes)",
                    tile.value.len()
                ));
            }
            if let Some(trend) = tile.trend.as_deref()
                && trend.len() > MAX_TILE_FIELD_BYTES
            {
                return Err(format!(
                    "tile[{i}].trend exceeds {MAX_TILE_FIELD_BYTES}-byte cap (got {} bytes)",
                    trend.len()
                ));
            }
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
    ///
    /// PR 38 (codex review): the success-body read is capped at
    /// [`MAX_RESPONSE_BYTES`] so a misbehaving or attacker-controlled
    /// rasterizer can't flood the adapter's memory before the chat
    /// courier ever runs. The stream is consumed chunk-by-chunk and
    /// aborted with [`RasterizerError::Server`] the moment the cap
    /// is breached.
    pub async fn render(&self, req: &DashboardRequest) -> Result<Vec<u8>, RasterizerError> {
        let url = format!("{}/v1/dashboard.png", self.base);
        let mut resp = self.http.post(&url).json(req).send().await.map_err(|e| {
            if e.is_timeout() {
                RasterizerError::Timeout
            } else {
                RasterizerError::Network(e.to_string())
            }
        })?;
        let status = resp.status();
        if status.is_success() {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let chunk = resp
                    .chunk()
                    .await
                    .map_err(|e| RasterizerError::Network(e.to_string()))?;
                let Some(chunk) = chunk else { break };
                if buf.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(RasterizerError::Server(format!(
                        "response too large (exceeds {MAX_RESPONSE_BYTES}-byte cap)"
                    )));
                }
                buf.extend_from_slice(&chunk);
            }
            return Ok(buf);
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

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::DashboardTile;

    #[test]
    fn validate_accepts_within_caps() {
        let req = DashboardRequest {
            title: "ok".into(),
            tiles: vec![DashboardTile {
                label: "a".into(),
                value: "1".into(),
                trend: Some("+1".into()),
            }],
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn validate_rejects_oversize_tile_value() {
        // PR 38 codex review: a tile string past 128 bytes must
        // be refused at the rasterizer edge with a clear error
        // naming the offending field.
        let req = DashboardRequest {
            title: "ok".into(),
            tiles: vec![DashboardTile {
                label: "a".into(),
                value: "x".repeat(MAX_TILE_FIELD_BYTES + 1),
                trend: None,
            }],
        };
        let err = req.validate().expect_err("oversize value must reject");
        assert!(
            err.contains("tile[0].value") && err.contains(&format!("{MAX_TILE_FIELD_BYTES}")),
            "expected a per-field error mentioning the cap; got: {err}",
        );
    }

    #[test]
    fn validate_rejects_oversize_tile_label() {
        let req = DashboardRequest {
            title: "ok".into(),
            tiles: vec![DashboardTile {
                label: "L".repeat(MAX_TILE_FIELD_BYTES + 1),
                value: "1".into(),
                trend: None,
            }],
        };
        let err = req.validate().expect_err("oversize label must reject");
        assert!(err.contains("tile[0].label"), "got: {err}");
    }

    #[test]
    fn validate_rejects_oversize_tile_trend() {
        let req = DashboardRequest {
            title: "ok".into(),
            tiles: vec![DashboardTile {
                label: "a".into(),
                value: "1".into(),
                trend: Some("t".repeat(MAX_TILE_FIELD_BYTES + 1)),
            }],
        };
        let err = req.validate().expect_err("oversize trend must reject");
        assert!(err.contains("tile[0].trend"), "got: {err}");
    }

    #[test]
    fn validate_accepts_max_tile_field_size_exact() {
        // Boundary: a string exactly at the cap is accepted.
        let req = DashboardRequest {
            title: "ok".into(),
            tiles: vec![DashboardTile {
                label: "a".repeat(MAX_TILE_FIELD_BYTES),
                value: "v".repeat(MAX_TILE_FIELD_BYTES),
                trend: Some("t".repeat(MAX_TILE_FIELD_BYTES)),
            }],
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn max_response_bytes_is_2_mib() {
        // PR 38 codex review: the client-side cap is documented as
        // a constant so operators can reason about it without
        // reading the render path. 2 MiB is the chosen value;
        // anything different here is a load-bearing change that
        // needs review.
        assert_eq!(MAX_RESPONSE_BYTES, 2 * 1024 * 1024);
    }
}
