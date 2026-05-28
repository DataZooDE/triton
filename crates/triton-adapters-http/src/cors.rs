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
///
/// Credentials: `Access-Control-Allow-Credentials: true` is emitted
/// so a browser fetch with `credentials: "include"` (or Dio's web
/// `withCredentials = true`) can carry the oauth2-proxy session
/// cookie cross-origin to Triton — the wire shape the upcoming
/// internal-SSO sidecar deployment uses (SPA at one FQDN, REST
/// adapter at another, shared cookie-domain). Browsers refuse this
/// header in combination with `Allow-Origin: *`, which is why
/// `is_valid_origin` rejects `*` outright.
pub fn build_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter(|o| is_valid_origin(o))
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
            .allow_credentials(true)
            .max_age(Duration::from_secs(600)),
    )
}

/// A configured CORS origin must be an explicit `scheme://host[:port]`.
/// We refuse a literal `*` (it is not honoured as a wildcard by the
/// allow-list layer and almost always signals operator confusion) and
/// anything without an `http(s)://` scheme. Rejected entries are
/// dropped with a warning rather than silently mounted.
fn is_valid_origin(origin: &str) -> bool {
    if origin == "*" {
        tracing::warn!(
            "ignoring CORS origin `*`: wildcard origins are not supported; \
             list explicit `scheme://host` origins instead"
        );
        return false;
    }
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        tracing::warn!(origin = %origin, "ignoring CORS origin without an http(s):// scheme");
        return false;
    }
    true
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

#[cfg(test)]
mod tests {
    use super::{build_layer, is_valid_origin};

    #[test]
    fn rejects_wildcard_and_schemeless_origins() {
        assert!(!is_valid_origin("*"));
        assert!(!is_valid_origin("example.com"));
        assert!(is_valid_origin("https://example.com"));
        assert!(is_valid_origin(
            "http://dz-triton-explorer.service.consul:8080"
        ));
    }

    #[test]
    fn build_layer_none_when_only_invalid_origins() {
        assert!(build_layer(&["*".to_string()]).is_none());
        assert!(build_layer(&["not-an-origin".to_string()]).is_none());
        assert!(build_layer(&["https://ok.example".to_string()]).is_some());
    }
}
