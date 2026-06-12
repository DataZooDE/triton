//! Mode 2 (issue #75): `TRITON_STATIC_UPSTREAMS` lets one real `triton`
//! binary front a single agent endpoint with **no Consul, no Vault**.
//! No mocks: a real triton + a real FakeAgent over real HTTP.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_upstream_dispatch_no_hashicorp() {
    let agent = FakeAgent::start_echoing().await;

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        // No TRITON_CONSUL_URL / TRITON_VAULT_URL — just a static map.
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let http = reqwest::Client::new();

    // The agent isn't in the in-process registry, but /v1/tools lists it
    // (StaticUpstream::list_agents) flagged upstream.
    let tools: Value = http
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let listed = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "assistant")
        .expect("assistant listed");
    assert_eq!(listed["upstream"], json!(true));

    // And it dispatches to the static endpoint.
    let resp: Value = http
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": "static-42" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant")
        .json()
        .await
        .expect("json");
    assert_eq!(
        resp["result"]["echoed"]["marker"], "static-42",
        "static upstream echoed the args: {resp}"
    );

    // The agent saw the static dev-token (no Vault swap happened).
    assert_eq!(agent.bearers_seen()[0], "dev-token");

    // Contract parity with the Consul-mode router (#101): static
    // dispatch carries the informational `X-Triton-Tool` header too,
    // so an agent serving several tools can route without parsing
    // the args body.
    assert_eq!(
        agent.tools_seen()[0].as_deref(),
        Some("assistant"),
        "static upstream dispatch must carry X-Triton-Tool"
    );
}
