//! CORS preflight + actual-request behaviour, gated by
//! `TRITON_CORS_ALLOWED_ORIGINS`. Triton fronts internal tooling
//! (e.g. the Flutter explorer) running on a different origin; the
//! browser will refuse to read the response unless the gateway
//! echoes the allow-origin header for the registered origin and
//! 204s the `OPTIONS` preflight.
//!
//! Acceptance:
//!   * REST, MCP, A2A all consult the same allow-list.
//!   * An `OPTIONS` request from an **allowed** origin returns 204
//!     with `Access-Control-Allow-Origin` echoed and the requested
//!     methods/headers permitted.
//!   * An `OPTIONS` from an **unknown** origin does NOT echo
//!     `Access-Control-Allow-Origin` (the browser will then block
//!     the actual call).
//!   * When the env var is unset (default), no CORS headers are
//!     emitted — production parity for the substrate where the
//!     explorer isn't enabled.
//!
//! No mocks: real spawned binary, real HTTP, real preflight.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Method;
use reqwest::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN,
};
use triton_tests::TritonProcess;

const ALLOWED: &str = "https://explorer.local";
const DISALLOWED: &str = "https://evil.example";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_allowed_origin_passes_on_all_three_adapters() {
    let env = HashMap::from([(
        "TRITON_CORS_ALLOWED_ORIGINS".to_string(),
        ALLOWED.to_string(),
    )]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    for (url, expected_method) in [
        (proc.rest_url("/v1/tools"), "GET"),
        (proc.mcp_url("/"), "POST"),
        (proc.a2a_url("/message:send"), "POST"),
    ] {
        let resp = reqwest::Client::new()
            .request(Method::OPTIONS, &url)
            .header(ORIGIN, ALLOWED)
            .header(ACCESS_CONTROL_REQUEST_METHOD, expected_method)
            .header(ACCESS_CONTROL_REQUEST_HEADERS, "authorization,content-type")
            .send()
            .await
            .unwrap_or_else(|e| panic!("OPTIONS {url} failed: {e}"));

        assert!(
            resp.status().is_success(),
            "preflight {url} should succeed for allowed origin, got {}",
            resp.status()
        );
        let allow_origin = resp
            .headers()
            .get(ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            allow_origin, ALLOWED,
            "Access-Control-Allow-Origin must echo the allowed origin for {url}"
        );
        let allow_methods = resp
            .headers()
            .get(ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            allow_methods.to_uppercase().contains(expected_method),
            "Access-Control-Allow-Methods on {url} must include {expected_method}, got `{allow_methods}`"
        );
        let allow_headers = resp
            .headers()
            .get(ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        assert!(
            allow_headers.contains("authorization"),
            "Access-Control-Allow-Headers on {url} must include authorization, got `{allow_headers}`"
        );
        assert!(
            allow_headers.contains("content-type"),
            "Access-Control-Allow-Headers on {url} must include content-type, got `{allow_headers}`"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_unknown_origin_does_not_echo_allow_origin() {
    let env = HashMap::from([(
        "TRITON_CORS_ALLOWED_ORIGINS".to_string(),
        ALLOWED.to_string(),
    )]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .request(Method::OPTIONS, proc.rest_url("/v1/tools"))
        .header(ORIGIN, DISALLOWED)
        .header(ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .header(ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .send()
        .await
        .expect("OPTIONS for disallowed origin");

    let allow_origin = resp
        .headers()
        .get(ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        allow_origin.is_empty() || allow_origin == "null",
        "Access-Control-Allow-Origin must NOT echo the disallowed origin, got `{allow_origin}`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_get_from_allowed_origin_carries_allow_origin_header() {
    // The preflight is hop-1; the actual request is hop-2. tower-http
    // attaches Access-Control-Allow-Origin on the actual response too
    // so the browser will let JS read the body. /healthz is anonymous
    // so we don't need a token for this check.
    let env = HashMap::from([(
        "TRITON_CORS_ALLOWED_ORIGINS".to_string(),
        ALLOWED.to_string(),
    )]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .header(ORIGIN, ALLOWED)
        .send()
        .await
        .expect("GET /healthz");
    assert!(resp.status().is_success(), "GET /healthz status");
    let allow_origin = resp
        .headers()
        .get(ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        allow_origin, ALLOWED,
        "actual response should carry Access-Control-Allow-Origin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cors_headers_when_env_unset() {
    // Production parity: when TRITON_CORS_ALLOWED_ORIGINS isn't set,
    // the layer must NOT be mounted — no headers leak.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .header(ORIGIN, ALLOWED)
        .send()
        .await
        .expect("GET /healthz");
    assert!(resp.status().is_success());
    assert!(
        resp.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "no CORS header should leak when env var unset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_origins_comma_separated_all_pass_preflight() {
    let other = "https://nonprod.tailnet.ts.net";
    let env = HashMap::from([(
        "TRITON_CORS_ALLOWED_ORIGINS".to_string(),
        format!("{ALLOWED},{other}"),
    )]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    for origin in [ALLOWED, other] {
        let resp = reqwest::Client::new()
            .request(Method::OPTIONS, proc.rest_url("/v1/tools"))
            .header(ORIGIN, origin)
            .header(ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .send()
            .await
            .expect("OPTIONS");
        let echoed = resp
            .headers()
            .get(ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(echoed, origin, "origin {origin} must be allowed");
    }
}
