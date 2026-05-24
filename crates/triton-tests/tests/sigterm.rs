//! ACC-2 (SIGTERM drain) — clean exit on SIGTERM, and (FR-L-3)
//! identical behaviour on SIGINT. PR 2 asserts the clean-exit half;
//! the mid-flight 5xx-free assertion lands once we have a slow
//! endpoint (PR 11).
//!
//! No mocks: real signal, real process, real TCP teardown.

use std::time::Duration;

use triton_tests::{Signal, TritonProcess};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_drains_and_exits_zero() {
    assert_clean_exit_on_signal(Signal::Term).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_drains_and_exits_zero() {
    assert_clean_exit_on_signal(Signal::Int).await;
}

async fn assert_clean_exit_on_signal(sig: Signal) {
    let mut proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    // Sanity-check that the listeners actually came up before we
    // shut them down — otherwise the drain test could pass on a
    // process that never bound.
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("pre-drain /healthz");
    assert!(resp.status().is_success());

    let status = proc.signal(sig, Duration::from_secs(5));

    assert!(
        status.success(),
        "triton exited non-zero on {sig:?}: {status:?}"
    );
}
