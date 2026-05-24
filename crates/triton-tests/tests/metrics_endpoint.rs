//! `GET /v1/metrics` — Prometheus text exposition served by the
//! REST adapter. Same body the tailnet-only `:9090` listener
//! serves; the difference is auth (the REST route requires the
//! same Bearer as `/v1/tools`) and CORS (REST is the only listener
//! the explorer SPA can talk to).
//!
//! Acceptance:
//!   * Returns text/plain, content matches Prometheus text format.
//!   * Each successful dispatch produces a `triton_dispatch_total`
//!     counter increment with the right `{tool, protocol, result}`
//!     labels.
//!   * Each emitted audit line produces a `triton_audit_lines_total`
//!     counter increment with the right `phase` label.
//!   * Auth required.
//!
//! No mocks: real binary, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_exposes_process_up_and_counters() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let client = reqwest::Client::new();

    // Fire one echo and one bogus-token call so the counters have
    // something interesting to expose.
    let _ = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({ "message": "metrics-test" }))
        .send()
        .await
        .expect("POST echo");
    let _ = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("not-a-real-token")
        .json(&serde_json::json!({ "message": "x" }))
        .send()
        .await;

    let resp = client
        .get(proc.rest_url("/v1/metrics"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/metrics");
    assert!(resp.status().is_success());
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/plain"),
        "content-type should be text/plain, got `{ct}`"
    );
    let body = resp.text().await.expect("text body");

    // Process gauge is always present.
    assert!(
        body.contains("triton_process_up 1"),
        "missing process_up gauge in:\n{body}"
    );
    // Dispatch counter for our successful echo.
    assert!(
        body.contains("triton_dispatch_total{")
            && body.contains("tool=\"echo\"")
            && body.contains("protocol=\"rest\""),
        "missing dispatch counter for echo in:\n{body}"
    );
    // Audit line counter for at least one of the phases we drove.
    assert!(
        body.contains("triton_audit_lines_total"),
        "missing audit counter in:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_requires_auth() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/metrics"))
        .send()
        .await
        .expect("GET /v1/metrics");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the REST /v1/metrics route MUST require a Bearer (G-7 stays \
         tailnet-only on :9090)"
    );
}
