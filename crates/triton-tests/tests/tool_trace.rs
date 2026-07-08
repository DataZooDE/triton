//! Per-message tool trace reflection.
//!
//! An upstream agent may attach a per-turn tool trace under
//! `_meta.tool_trace` (which tools ran + the Escurel data they touched) — a
//! host-render debug sidecar the Explorer shows in a per-message sidebar.
//! Triton mirrors it to the client on every buffered transport, exactly like
//! `_meta.ui`: MCP on the response `_meta`, REST as an envelope sibling, A2A
//! on `metadata`. Only a structured ARRAY is reflected — a scalar/object blob
//! a hostile upstream might stuff there is dropped (mirrors the `_meta.ui`
//! MEDIUM guard from #143).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

/// The trace an agent would emit for a "top customers" turn: a knowledge
/// search then the authored query page, both reads.
fn trace_body() -> Value {
    json!({
        "surface": { "components": [ { "kind": "text", "value": "Initech leads." } ] },
        "_meta": { "tool_trace": [
            { "tool": "search_knowledge", "target": "top customers", "verb": "read", "ms": 12 },
            { "tool": "run_query", "target": "top_customers", "verb": "read", "ms": 34 }
        ] }
    })
}

async fn spawn_for(agent: &FakeAgent) -> TritonProcess {
    TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("assistant={}", agent.host_port()),
            ),
        ]),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_trace_reflects_on_mcp_and_rest() {
    let agent = FakeAgent::start_returning(trace_body()).await;
    let proc = spawn_for(&agent).await;
    let http = reqwest::Client::new();

    // MCP — reflected onto the tools/call response `_meta.tool_trace`.
    let mcp: Value = http
        .post(format!("http://{}/", proc.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "assistant", "arguments": { "question": "top customers?" } }
        }))
        .send()
        .await
        .expect("POST mcp")
        .json()
        .await
        .expect("decode mcp");
    let mcp_trace = &mcp["result"]["_meta"]["tool_trace"];
    assert_eq!(mcp_trace.as_array().map(Vec::len), Some(2), "mcp: {mcp}");
    assert_eq!(mcp_trace[0]["tool"], "search_knowledge");
    assert_eq!(mcp_trace[1]["target"], "top_customers");
    assert_eq!(mcp_trace[1]["verb"], "read");
    // The trace_id still rides `_meta` next to it (unchanged behaviour).
    assert!(mcp["result"]["_meta"]["trace_id"].is_string());

    // REST — reflected as an envelope sibling (`tool_trace`), like `trace_id`.
    let rest: Value = http
        .post(proc.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "question": "top customers?" }))
        .send()
        .await
        .expect("POST rest")
        .json()
        .await
        .expect("decode rest");
    let rest_trace = &rest["tool_trace"];
    assert_eq!(rest_trace.as_array().map(Vec::len), Some(2), "rest: {rest}");
    assert_eq!(rest_trace[1]["tool"], "run_query");
    assert_eq!(rest_trace[0]["verb"], "read");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_array_tool_trace_is_not_reflected() {
    // A hostile/confused upstream stuffs a scalar under `_meta.tool_trace`.
    let agent = FakeAgent::start_returning(json!({
        "surface": { "components": [ { "kind": "text", "value": "hi" } ] },
        "_meta": { "tool_trace": "gotcha" }
    }))
    .await;
    let proc = spawn_for(&agent).await;
    let http = reqwest::Client::new();

    let mcp: Value = http
        .post(format!("http://{}/", proc.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "assistant", "arguments": { "question": "hi" } }
        }))
        .send()
        .await
        .expect("POST mcp")
        .json()
        .await
        .expect("decode mcp");
    // The scalar is dropped — no `tool_trace` key leaks onto the response.
    assert!(
        mcp["result"]["_meta"].get("tool_trace").is_none(),
        "scalar tool_trace must not be reflected: {mcp}"
    );
}
