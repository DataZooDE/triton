//! Issue #375 (2): an upstream's result `_meta` reaches an MCP host.
//!
//! Triton's MCP `tools/call` answer builds its own `_meta` (`trace_id`, the
//! card's `triton/*` keys). An upstream's own `_meta` keys (a chart image, a
//! neutral chart summary, an MCP App's data) are passed through next to
//! them, so an upstream MCP App widget behind Triton gets what it reads.
//! The rule that keeps this safe: an upstream never sets a Triton-owned or
//! MCP-reserved key, and the passed-through keys share a size budget.
//!
//! Real binary, real TCP, a real upstream (`FakeAgent`). No mocks.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

async fn spawn_for(agent: &FakeAgent) -> TritonProcess {
    TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_report={}", agent.host_port()),
            ),
        ]),
    )
    .await
}

async fn call(proc: &TritonProcess) -> Value {
    reqwest::Client::new()
        .post(format!("http://{}/", proc.mcp_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "render_report", "arguments": { "report_id": "revenue" } }
        }))
        .send()
        .await
        .expect("POST mcp")
        .json()
        .await
        .expect("decode mcp")
}

fn chart() -> Value {
    json!({ "kind": "bar", "x": "region", "y": "revenue",
            "rows": [ { "region": "North", "revenue": 10 } ] })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_meta_keys_pass_through_but_never_override_tritons() {
    let agent = FakeAgent::start_returning(json!({
        "content": [ { "type": "text", "text": "Revenue by region" } ],
        "structuredContent": { "facts": [] },
        "_meta": {
            "png_base64": "iVBORw0KGgo=",
            "svgs": [ "<svg/>" ],
            "chart": chart(),
            "vega_specs": [ { "mark": "bar" } ],
            "openai/widgetAccessible": true,
            // Protected: an upstream must not set any of these.
            "trace_id": "forged-trace",
            "triton/a2ui": [ { "forged": true } ],
            "triton/in_card": "forged-token",
            "triton/future": 1,
            "io.modelcontextprotocol/related-task": { "taskId": "upstream-task" },
            "tasks.mcp.example/hint": 1
        }
    }))
    .await;
    let proc = spawn_for(&agent).await;
    let mcp = call(&proc).await;
    let meta = &mcp["result"]["_meta"];

    // Passed through verbatim, at the top level of the result `_meta`.
    assert_eq!(meta["png_base64"], "iVBORw0KGgo=", "{mcp}");
    assert_eq!(meta["svgs"], json!(["<svg/>"]));
    assert_eq!(meta["chart"], chart());
    assert_eq!(meta["vega_specs"][0]["mark"], "bar");
    assert_eq!(meta["openai/widgetAccessible"], true);

    // Triton's own keys stay Triton's.
    let trace = meta["trace_id"].as_str().expect("trace_id");
    assert_ne!(trace, "forged-trace", "{meta}");
    assert!(!trace.is_empty());
    for k in [
        "triton/a2ui",
        "triton/in_card",
        "triton/future",
        "io.modelcontextprotocol/related-task",
        "tasks.mcp.example/hint",
    ] {
        assert!(meta.get(k).is_none(), "protected key {k} leaked: {meta}");
    }
    // The model-visible fields are unchanged by the pass-through.
    assert_eq!(mcp["result"]["isError"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_meta_beyond_the_size_budget_is_dropped_key_by_key() {
    // 5 MiB of base64 exceeds the 4 MiB budget on its own; the small keys
    // around it still pass.
    let big = "A".repeat(5 * 1024 * 1024);
    let agent = FakeAgent::start_returning(json!({
        "content": [ { "type": "text", "text": "big" } ],
        "_meta": { "small": 1, "png_base64": big, "chart": chart() }
    }))
    .await;
    let proc = spawn_for(&agent).await;
    let mcp = call(&proc).await;
    let meta = &mcp["result"]["_meta"];
    assert!(meta.get("png_base64").is_none(), "over-budget key dropped");
    assert_eq!(meta["small"], 1);
    assert_eq!(meta["chart"], chart());
    assert!(meta["trace_id"].is_string());
    assert_eq!(mcp["result"]["isError"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_object_upstream_meta_is_ignored() {
    let agent = FakeAgent::start_returning(json!({
        "content": [ { "type": "text", "text": "odd" } ],
        "_meta": "not-an-object"
    }))
    .await;
    let proc = spawn_for(&agent).await;
    let mcp = call(&proc).await;
    let meta = mcp["result"]["_meta"].as_object().expect("meta object");
    assert!(meta.contains_key("trace_id"));
    assert_eq!(meta.len(), 1, "only Triton's key: {meta:?}");
}
