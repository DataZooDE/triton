//! A2UI v0.8 + v0.9 envelope parity across MCP/A2A/REST — ACC-1.
//!
//! Drives the `narrate` tool through every HTTP-trio adapter at
//! each negotiated version and asserts the produced envelope is
//! byte-equal after parsing (FR-A-4, ADR-4 — version negotiation
//! lives only at the envelope layer).
//!
//! No mocks: real binary, real HTTP, real JSON.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;

async fn rest_envelope(proc: &TritonProcess, accept: &str) -> Value {
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/narrate"))
        .bearer_auth("dev-token")
        .header("Accept", accept)
        .json(&json!({ "subject": "agents" }))
        .send()
        .await
        .expect("rest");
    assert!(resp.status().is_success(), "REST {}", resp.status());
    resp.json::<Value>().await.expect("rest decode")["result"].clone()
}

async fn a2a_envelope(proc: &TritonProcess, version: &str) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "narrate", "args": { "subject": "agents" } } }],
            "metadata": { "a2ui_version": version }
        }))
        .send()
        .await
        .expect("a2a");
    assert!(resp.status().is_success(), "A2A {}", resp.status());
    let body: Value = resp.json().await.expect("a2a decode");
    body["parts"][0]["data"]["result"].clone()
}

async fn mcp_envelope(proc: &TritonProcess, version: &str) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("http://{}/", proc.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "narrate",
                "arguments": { "subject": "agents" },
                "_meta": { "a2ui_version": version }
            }
        }))
        .send()
        .await
        .expect("mcp");
    assert!(resp.status().is_success(), "MCP {}", resp.status());
    let body: Value = resp.json().await.expect("mcp decode");
    body["result"]["structuredContent"]["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acc1_three_way_parity_v0_8() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let rest = rest_envelope(&proc, "application/json+a2ui").await; // v0.8 default
    let a2a = a2a_envelope(&proc, "v0.8").await;
    let mcp = mcp_envelope(&proc, "v0.8").await;
    assert_eq!(rest["version"], "0.8");
    assert_eq!(rest, a2a, "REST != A2A for v0.8");
    assert_eq!(rest, mcp, "REST != MCP for v0.8");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acc1_three_way_parity_v0_9() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let rest = rest_envelope(&proc, "application/json+a2ui; version=0.9").await;
    let a2a = a2a_envelope(&proc, "v0.9").await;
    let mcp = mcp_envelope(&proc, "v0.9").await;
    assert_eq!(rest["version"], "0.9");
    assert_eq!(rest, a2a, "REST != A2A for v0.9");
    assert_eq!(rest, mcp, "REST != MCP for v0.9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v08_envelope_uses_pascalcase_component_wrapper() {
    // ADR-4 / experiment finding: v0.8 wraps each component in a
    // `Component` field carrying a typed inner object.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let env = rest_envelope(&proc, "application/json+a2ui").await;
    assert_eq!(env["version"], "0.8");
    let stream = env["stream"]
        .as_array()
        .unwrap_or_else(|| panic!("stream is array: {env}"));
    assert!(!stream.is_empty());
    assert!(
        stream[0]["Component"].is_object(),
        "v0.8 must use PascalCase Component wrapper: {}",
        stream[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v09_envelope_is_flat() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let env = rest_envelope(&proc, "application/json+a2ui; version=0.9").await;
    assert_eq!(env["version"], "0.9");
    let stream = env["stream"]
        .as_array()
        .unwrap_or_else(|| panic!("stream is array: {env}"));
    assert!(!stream.is_empty());
    // v0.9: flat components, `type` field is lowercase.
    assert!(
        stream[0]["type"].is_string(),
        "v0.9 must be flat with `type`: {}",
        stream[0]
    );
    assert!(
        stream[0]["Component"].is_null(),
        "v0.9 must NOT carry the v0.8 `Component` wrapper: {}",
        stream[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_json_when_a2ui_not_requested() {
    // No Accept header → caller gets the raw tool result, no
    // envelope wrap. This is the contract for non-A2UI clients
    // (REST CLI tools, integration tests, etc.).
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/narrate"))
        .bearer_auth("dev-token")
        .json(&json!({ "subject": "agents" }))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.expect("decode");
    // Raw tool result, not wrapped.
    assert!(
        body["result"]["surface"].is_object(),
        "expected raw tool result with `surface` field: {body}"
    );
    assert!(
        body["result"]["version"].is_null(),
        "raw result must not have an A2UI `version` field: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_passthrough_when_tool_does_not_return_a2ui() {
    // `echo`'s descriptor says `returns_a2ui = false`, so even when
    // the client requests A2UI the adapter MUST NOT wrap (FR-A-5 —
    // only tools that opted in receive the envelope).
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["echo"], "hi");
    assert!(
        body["result"]["version"].is_null(),
        "echo MUST NOT receive an A2UI envelope: {body}"
    );
}
