//! ACC-8 (cold start) — fresh process boots and `/healthz` returns 200.
//!
//! This is the first walking-skeleton test. No mocks: a real binary,
//! a real TCP listener, a real HTTP client. If this test fails the
//! task is not done.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_returns_ok_on_cold_start() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("GET /healthz")
        .json()
        .await
        .expect("decode JSON");

    assert_eq!(body["status"], "ok", "unexpected /healthz body: {body}");
}
