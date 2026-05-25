//! `frontend → triton → app-agent` integration test.
//!
//! Boots a REAL `triton` binary (no mocks), points it at a
//! `FakeConsul` that resolves your tool to a `FakeAgent`, and drives
//! it as a frontend would. The upstream router needs Vault to mint
//! the per-call agent token, so a `FakeVault` is required alongside
//! Consul — the binary refuses to boot with one but not the other.
//!
//! Mirrors crates/triton-tests/tests/upstream.rs. See references/08.
//!
//! Prereq: the `triton` binary must already be built (cargo build in
//! the Triton checkout, or CARGO_BIN_EXE_triton set).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use triton_tests::{
    TritonProcess,
    upstream_fixture::{FakeAgent, FakeConsul, FakeVault},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_calls_triton_calls_my_agent() {
    // 1. Your agent. In prod this is your Nomad job; here a fake that
    //    echoes the args body back. Swap for FakeAgent profiles to
    //    exercise failure paths (start_always_failing,
    //    start_failing_then_recovering).
    let agent = FakeAgent::start_echoing().await;

    // 2. Consul resolves `tag:agent:my-tool` to the fake agent.
    //    EDIT: "my-tool" must match the tool name your frontend calls.
    let consul = FakeConsul::start(&[("my-tool", agent.host_port())]).await;

    // 3. Vault mints the per-call OIDC token Triton sends your agent.
    let vault = FakeVault::start_minting("vault-minted-agent-token").await;

    // 4. Spawn the real Triton. Consul + Vault env go together.
    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        ("TRITON_CONSUL_URL".into(), consul.url()),
        ("TRITON_VAULT_URL".into(), vault.url()),
        ("TRITON_VAULT_TOKEN".into(), "triton-vault-token".into()),
        ("TRITON_VAULT_OIDC_ROLE".into(), "agent-oidc-swap".into()),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // 5. Drive it like a frontend (references/06). dev-token works
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

    // The lethal-trifecta cut: your agent saw the Vault-minted token,
    // NOT the frontend's dev-token (FR-U-2, NFR-S-3).
    let seen = agent.bearers_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], "vault-minted-agent-token");
    assert!(!seen[0].contains("dev-token"));
}
