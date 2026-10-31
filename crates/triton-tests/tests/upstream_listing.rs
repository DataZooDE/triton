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

/// The upstream's own `description` and `annotations` (MCP tool
/// descriptor fields) reach MCP clients: a model picks tools by their
/// description, and a missing one makes an upstream tool unusable for
/// Claude or ChatGPT. `/v1/tools` carries the same fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_passes_upstream_description_and_annotations() {
    let agent = FakeAgent::start_mcp_apps().await;
    let env = HashMap::from([(
        "TRITON_STATIC_UPSTREAMS".into(),
        format!("render_report={}", agent.host_port()),
    )]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let client = reqwest::Client::new();

    let body: Value = client
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
    let rr = tools
        .iter()
        .find(|t| t["name"] == "render_report")
        .expect("render_report in MCP tools/list");
    assert_eq!(
        rr["description"],
        json!("Render a named report as an interactive view."),
        "upstream description must reach MCP clients; got: {rr}"
    );
    assert_eq!(
        rr["annotations"],
        json!({ "readOnlyHint": true, "title": "Render report" }),
        "upstream annotations must reach MCP clients; got: {rr}"
    );
    assert_eq!(
        rr["inputSchema"]["required"],
        json!(["report_id"]),
        "the schema is still passed"
    );

    let body: Value = client
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let rr = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|t| t["name"] == "render_report")
        .cloned()
        .expect("render_report in /v1/tools");
    assert_eq!(
        rr["description"],
        json!("Render a named report as an interactive view.")
    );
    assert_eq!(rr["annotations"]["readOnlyHint"], json!(true));
}

/// An upstream that advertises no description gets no `description`
/// key (not `null`): MCP declares the field optional, and a `null`
/// string trips strict clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_omits_description_an_upstream_does_not_give() {
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
    let hello = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|t| t["name"] == "hello")
        .cloned()
        .expect("hello listed");
    assert!(hello.get("description").is_none(), "got: {hello}");
    assert!(hello.get("annotations").is_none(), "got: {hello}");
}
