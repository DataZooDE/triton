//! CORS layer for the HTTP trio.
//!
//! Triton's adapters can be called cross-origin by internal tooling
//! that lives on a different host (e.g. the Flutter explorer at
//! `apps/explorer/`). The browser will refuse to surface the
//! response body unless the server echoes
//! `Access-Control-Allow-Origin` for the calling origin and 204s
//! the `OPTIONS` preflight.
//!
//! Production parity: when `TRITON_CORS_ALLOWED_ORIGINS` is unset
//! (the default), no layer is mounted — the gateway behaves exactly
//! as it did before this feature landed. Only opt-in deployments
//! that explicitly enable the explorer get cross-origin access.

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Build a permissive-but-allow-listed `CorsLayer` for an explicit
/// set of origins. Returns `None` when the list is empty so callers
/// can skip mounting the layer entirely (avoids attaching a no-op
/// middleware on every request).
///
/// Methods: GET / POST / OPTIONS — the three Triton actually serves.
/// Headers: `authorization`, `content-type`, `accept` — the three
/// the SPA actually sends. Max-age 10 minutes so the browser caches
/// preflights instead of probing per call.
pub fn build_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(parsed))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
                HeaderName::from_static("accept"),
            ])
            .max_age(Duration::from_secs(600)),
    )
}

/// Parse a comma-separated `TRITON_CORS_ALLOWED_ORIGINS` value into
/// a clean list, trimming whitespace and dropping empty entries.
pub fn parse_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
