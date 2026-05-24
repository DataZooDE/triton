//! FR-O-3 / G-5 / G-7 — `/metrics` lives on its own listener,
//! tailnet-only. The public REST listener MUST NOT expose it.

use std::collections::HashMap;
use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_served_on_dedicated_port_in_prometheus_shape() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let metrics_addr = proc.metrics_addr.expect("metrics listener bound");

    let resp = reqwest::Client::new()
        .get(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .expect("GET /metrics");
    assert!(
        resp.status().is_success(),
        "GET /metrics: {}",
        resp.status()
    );
    let body = resp.text().await.expect("decode");
    // Prometheus exposition format starts with HELP/TYPE comments
    // and includes our process-up sentinel.
    assert!(
        body.contains("# HELP") || body.contains("# TYPE"),
        "expected Prometheus comments, got:\n{body}"
    );
    assert!(
        body.contains("triton_process_up"),
        "expected triton_process_up metric, got:\n{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_not_exposed_on_public_rest_listener() {
    // G-7 / NFR-S-2: /metrics is tailnet-only. The REST listener
    // (which Fabio routes to in production) MUST 404 it.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/metrics"))
        .send()
        .await
        .expect("GET /metrics on REST");
    assert_eq!(resp.status(), 404, "/metrics MUST NOT be reachable on REST");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_increments_on_dispatch() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let metrics_addr = proc.metrics_addr.expect("metrics listener bound");

    // Fire one successful dispatch.
    let _ = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({ "message": "x" }))
        .send()
        .await
        .expect("dispatch");

    // Give the metrics writer a moment to update.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let body = reqwest::Client::new()
        .get(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("decode");
    assert!(
        body.lines().any(|l| {
            l.starts_with("triton_dispatch_total")
                && l.contains(r#"tool="echo""#)
                && l.contains(r#"result="ok""#)
                && l.trim_end().ends_with(" 1")
        }),
        "expected `triton_dispatch_total{{tool=\"echo\",result=\"ok\",...}} 1`, got:\n{body}"
    );
}
