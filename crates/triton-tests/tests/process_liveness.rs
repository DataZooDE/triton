//! triton-bin must serve until SIGTERM/SIGINT — *not* self-exit
//! at the drain deadline.
//!
//! Regression for issue #65: an earlier shape wrapped the indefinite
//! `serve_*` join in `tokio::time::timeout(drain_deadline, …)`, which
//! caused the process to exit `drain_deadline` seconds after
//! `listeners bound`, regardless of signals or traffic. The substrate
//! `dz-triton-api` alloc restart-looped every 10 s (the deployed
//! `TRITON_DRAIN_DEADLINE_SECS`) until Nomad failed it.
//!
//! No mocks — real spawned binary, real HTTP. The harness's default
//! `TRITON_DRAIN_DEADLINE_SECS=3` (`triton-tests::TritonProcess`) is
//! overridden here so a future harness bump doesn't paper over a
//! regression.

use std::collections::HashMap;
use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_outlives_drain_deadline_without_signal() {
    // Short drain deadline so the test runs fast. The point: triton
    // must NOT exit at `start + drain_deadline` — it must keep serving
    // until the harness's `Drop` impl SIGTERMs it on tear-down.
    let env = HashMap::from([("TRITON_DRAIN_DEADLINE_SECS".to_string(), "2".to_string())]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // Wait well past 2 × drain_deadline. If the bug is back, triton
    // self-exited at +2 s and the curl below errors with connection
    // refused.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect(
            "/healthz must still respond 5 s after start with drain_deadline=2 s — \
             triton-bin must not self-terminate at the drain deadline (issue #65)",
        );
    assert!(
        resp.status().is_success(),
        "/healthz must be 2xx, got {}",
        resp.status()
    );
}
