//! A2A error→HTTP-status parity with REST (architecture §8.3).
//!
//! Codex correctness review: A2A mapped every `Tool` error to 502, so
//! a tripped circuit breaker returned 502 instead of 503. The status
//! now comes from the shared `TritonError::http_status()`, so A2A
//! agrees with REST. No mocks: real binary + real Consul/Vault/agent
//! fakes, driven over the A2A `POST /message:send` surface.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::{FakeAgent, FakeConsul, FakeVault};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_circuit_open_returns_503_like_rest() {
    let agent = FakeAgent::start_always_failing().await;
    let consul = FakeConsul::start(&[("flaky", agent.host_port())]).await;
    let vault = FakeVault::start_minting("t").await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_CONSUL_URL".to_string(), consul.url()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        (
            "TRITON_VAULT_TOKEN".to_string(),
            "triton-vault-token".to_string(),
        ),
        (
            "TRITON_VAULT_OIDC_ROLE".to_string(),
            "agent-oidc-swap".to_string(),
        ),
        ("TRITON_CIRCUIT_OPEN_AFTER".to_string(), "2".to_string()),
        ("TRITON_CIRCUIT_COOLDOWN_MS".to_string(), "200".to_string()),
        ("TRITON_UPSTREAM_TIMEOUT_MS".to_string(), "500".to_string()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/message:send", proc.a2a_addr);
    let msg = json!({
        "parts": [ { "data": { "tool": "flaky", "args": {} } } ]
    });

    // Two upstream failures fill the breaker. Each is a Tool error →
    // 502 in A2A (matches REST).
    for _ in 0..2 {
        let r = client
            .post(&url)
            .bearer_auth("dev-token")
            .json(&msg)
            .send()
            .await
            .expect("POST");
        assert_eq!(r.status(), 502, "upstream 500 should map to 502 in A2A");
    }

    // Breaker now open → circuit_open Tool error → 503, not 502.
    let r = client
        .post(&url)
        .bearer_auth("dev-token")
        .json(&msg)
        .send()
        .await
        .expect("POST");
    assert_eq!(
        r.status(),
        503,
        "A2A circuit_open must be 503 (architecture §8.3), got {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.expect("decode");
    assert_eq!(body["error"], "tool");
    assert!(
        body["message"].as_str().unwrap().contains("circuit_open"),
        "expected circuit_open marker: {body}"
    );
}
