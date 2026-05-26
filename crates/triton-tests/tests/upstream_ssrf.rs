//! SSRF guard on Consul-resolved upstream endpoints (NFR-S-4).
//!
//! A poisoned Consul catalog entry must not make Triton dial an
//! arbitrary host while carrying a freshly minted agent bearer. The
//! router refuses public / link-local targets (here the
//! 169.254.169.254 cloud-metadata address) before minting or dialing.
//! No mocks: real binary + real Consul/Vault fakes.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::{FakeConsul, FakeVault};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_refuses_link_local_metadata_endpoint() {
    // Consul claims the tool lives at the cloud-metadata IP.
    let consul = FakeConsul::start(&[("evil", "169.254.169.254:80".to_string())]).await;
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
        ("TRITON_UPSTREAM_TIMEOUT_MS".to_string(), "500".to_string()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/evil"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    // Provider error → 502, and the message names the SSRF refusal
    // (not a connection timeout), proving the guard fired before any
    // dial to the metadata endpoint.
    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "provider");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("not a permitted"),
        "expected SSRF-guard message, got: {body}"
    );
}
