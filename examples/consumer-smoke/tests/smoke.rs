//! ACC-13 external-consumer smoke (FR-T-1, FR-T-2).
//!
//! This test lives OUTSIDE the Triton workspace. It depends on
//! `triton-tests` via `path`, mirroring what a downstream app's
//! own CI would do. The shape is identical to
//! `crates/triton-tests/tests/consumer_smoke.rs` so a regression
//! in either path surfaces immediately, but this file is the one
//! that proves the harness is consumable from a non-workspace
//! Cargo project.
//!
//! Run with `cd examples/consumer-smoke && cargo test`.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_consumer_can_boot_triton_and_hit_tools_list() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let url = format!("http://{}/v1/tools", proc.rest_addr);
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", "Bearer dev-token")
        .send()
        .await
        .expect("GET /v1/tools");

    assert_eq!(resp.status(), 200, "dev-token MUST reach /v1/tools");
    let body: serde_json::Value = resp.json().await.expect("body json");
    assert!(
        body.get("tools").and_then(|t| t.as_array()).is_some(),
        "expected `tools` array; got: {body}"
    );
}
