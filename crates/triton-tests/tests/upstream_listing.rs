//! `GET /v1/tools` and MCP `tools/list` surface the upstream agents
//! named in `TRITON_STATIC_UPSTREAMS`, so a client (the Explorer) can
//! discover agents that aren't in Triton's in-process registry. No
//! mocks: a real `triton` binary against an in-repo `FakeAgent`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

fn upstream_env(agent: &FakeAgent) -> HashMap<String, String> {
    HashMap::from([(
        "TRITON_STATIC_UPSTREAMS".into(),
        format!("hello={}", agent.host_port()),
    )])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_v1_tools_lists_upstream_agents() {
    let agent = FakeAgent::start_echoing().await;
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), upstream_env(&agent)).await;

    let body: Value = reqwest::Client::new()
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let tools = body["tools"].as_array().expect("tools array");

    let hello = tools
        .iter()
        .find(|t| t["name"] == "hello")
        .expect("the upstream agent `hello` should be listed in /v1/tools");
    assert_eq!(
        hello["upstream"],
        json!(true),
        "upstream agents must be flagged so the UI can tag them"
    );

    // In-process tools are still listed, and NOT flagged upstream.
    let echo = tools
        .iter()
        .find(|t| t["name"] == "echo")
        .expect("in-process `echo` still listed");
    assert_eq!(echo["upstream"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tools_list_includes_upstream_agents() {
    let agent = FakeAgent::start_echoing().await;
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), upstream_env(&agent)).await;

    let body: Value = reqwest::Client::new()
        .post(triton.mcp_url("/"))
        .bearer_auth("dev-token")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}))
        .send()
        .await
        .expect("POST / tools/list")
        .json()
        .await
        .expect("json");
    let tools = body["result"]["tools"].as_array().expect("tools array");
    let hello = tools
        .iter()
        .find(|t| t["name"] == "hello")
        .expect("`hello` should appear in MCP tools/list");
    assert_eq!(hello["x-triton"]["upstream"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_tools_surfaces_upstream_input_schema_via_introspection() {
    // The agent answers `X-Triton-MCP: tools/list` with its real argument
    // schema; Triton introspects it so `/v1/tools` carries that schema (not
    // `{}`), letting the Explorer build an input form.
    let agent = FakeAgent::start_mcp_apps().await;
    let env = HashMap::from([(
        "TRITON_STATIC_UPSTREAMS".into(),
        format!("render_report={}", agent.host_port()),
    )]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let body: Value = reqwest::Client::new()
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let tools = body["tools"].as_array().expect("tools array");
    let rr = tools
        .iter()
        .find(|t| t["name"] == "render_report")
        .expect("render_report listed");
    assert_eq!(rr["upstream"], json!(true));
    assert_eq!(
        rr["input_schema"]["properties"]["report_id"]["type"],
        json!("string"),
        "upstream schema should be introspected into /v1/tools; got: {rr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listing_degrades_when_no_upstream_configured() {
    // Dev-token mode, no static upstreams: /v1/tools still works, just
    // the in-process tools, none flagged upstream.
    let triton = TritonProcess::spawn().await;
    let body: Value = reqwest::Client::new()
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let tools = body["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "in-process tools still listed");
    assert!(
        tools.iter().all(|t| t["upstream"] == json!(false)),
        "no upstream agents without a static map"
    );
}
