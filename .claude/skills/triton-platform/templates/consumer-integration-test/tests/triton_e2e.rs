//! `frontend → triton → app-agent` integration test.
//!
//! Boots a REAL `triton` binary (no mocks) and points it at your agent
//! with `TRITON_STATIC_UPSTREAMS=<tool>=<agent host:port>`, then drives
//! it as a frontend would. There is no Consul and no Vault: discovery
//! is the static name→endpoint map, and in dev-token mode Triton sends
//! the static `dev-token` as the upstream bearer (production mints an
//! RS256 JWT instead, verified against Triton's JWKS).
//!
//! Mirrors crates/triton-tests/tests/static_upstream.rs. See references/08.
//!
//! Prereq: the `triton` binary must already be built (cargo build in
//! the Triton checkout, or CARGO_BIN_EXE_triton set).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_calls_triton_calls_my_agent() {
    // 1. Your agent. In prod this is your real HTTP service; here a
    //    fake that echoes the args body back. Swap for other FakeAgent
    //    profiles to exercise failure paths (start_always_failing,
    //    start_failing_then_recovering).
    let agent = FakeAgent::start_echoing().await;

    // 2. Spawn the real Triton, resolving your tool to the agent via
    //    the static map. No Consul, no Vault — just name=host:port.
    //    EDIT: "my-tool" must match the tool name your frontend calls.
    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("my-tool={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // 3. Drive it like a frontend (references/06). dev-token works
    //    because no OIDC issuer is configured (references/07).
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/my-tool"))
        .bearer_auth("dev-token")
        .json(&json!({ "city": "Berlin" }))
        .send()
        .await
        .expect("POST /v1/tools/my-tool");

    assert_eq!(resp.status(), 200, "expected 200 from Triton");

    let body: serde_json::Value = resp.json().await.expect("json body");
    // FakeAgent echoes the args; Triton wraps the upstream result in
    // `result`. EDIT: assert on YOUR agent's real response shape here.
    assert_eq!(body["result"]["echoed"]["city"], "Berlin");

    // In dev-token mode the agent saw the static dev-token (no Vault
    // swap — that path is gone). In prod Triton would mint an RS256
    // JWT here instead.
    let seen = agent.bearers_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], "dev-token");

    // The dispatch carries the informational `X-Triton-Tool` header so
    // a multi-tool agent can route without sniffing the body.
    assert_eq!(agent.tools_seen()[0].as_deref(), Some("my-tool"));
}
