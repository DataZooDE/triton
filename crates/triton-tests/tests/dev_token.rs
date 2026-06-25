//! `TRITON_DEV_TOKEN` — the accepted dev-token value is operator-settable
//! (no longer the hardcoded well-known `dev-token` literal).
//!
//! No mocks: a real spawned `triton` (dev-token build) over real HTTP.

use std::collections::HashMap;
use std::time::Duration;

use triton_tests::TritonProcess;

async fn get_tools_status(triton: &TritonProcess, bearer: &str) -> u16 {
    reqwest::Client::new()
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth(bearer)
        .send()
        .await
        .expect("GET /v1/tools")
        .status()
        .as_u16()
}

/// With `TRITON_DEV_TOKEN` set to a secret, that secret is accepted and the
/// historical `dev-token` literal is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_dev_token_is_accepted_and_default_is_rejected() {
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_DEV_TOKEN".into(), "s3cr3t-xyz".into())]),
    )
    .await;

    assert_eq!(
        get_tools_status(&triton, "s3cr3t-xyz").await,
        200,
        "the configured dev token must be accepted"
    );
    assert_eq!(
        get_tools_status(&triton, "dev-token").await,
        401,
        "the well-known `dev-token` literal must be rejected once overridden"
    );
}

/// Unset `TRITON_DEV_TOKEN` keeps the backward-compatible default so existing
/// dev workflows and tests are unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_dev_token_unchanged_when_unset() {
    let triton = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    assert_eq!(get_tools_status(&triton, "dev-token").await, 200);
    assert_eq!(get_tools_status(&triton, "nope").await, 401);
}

/// An empty `TRITON_DEV_TOKEN` is a kill-switch: every bearer is rejected,
/// even in a `dev-token` build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_dev_token_disables_the_path() {
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_DEV_TOKEN".into(), "".into())]),
    )
    .await;
    assert_eq!(
        get_tools_status(&triton, "dev-token").await,
        401,
        "empty TRITON_DEV_TOKEN must reject the default literal"
    );
    assert_eq!(
        get_tools_status(&triton, "").await,
        401,
        "empty TRITON_DEV_TOKEN must not accept an empty bearer"
    );
}
