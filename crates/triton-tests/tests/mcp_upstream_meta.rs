//! Issue #375 (2): an upstream's result `_meta` reaches an MCP host, by an
//! allow-list.
//!
//! Triton's MCP `tools/call` answer builds its own `_meta` (`trace_id`, the
//! card's `triton/*` keys). An upstream's own `_meta` keys pass through next
//! to them only when the allow-list names them: `ui` and `tool_trace` to
//! every client, the chart data (`chart`, `png_base64`, `svgs`, the
//! deprecated `vega_specs`) to a client that declared MCP Apps only, and
//! whatever the operator adds per tool in `adapter.yaml` (`tools.<name>.meta`).
//! Every other key is dropped; a Triton-owned or MCP-reserved key can never
//! be allowed, and the passed keys share a size budget.
//!
//! Real binary, real TCP, a real upstream (`FakeAgent`). No mocks.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

const UI: &str = "io.modelcontextprotocol/ui";

async fn spawn_for(agent: &FakeAgent) -> TritonProcess {
    spawn_with(agent, None).await
}

/// With `tools_yaml`, an `adapter.yaml` whose `tools:` section it is.
async fn spawn_with(agent: &FakeAgent, tools_yaml: Option<&str>) -> TritonProcess {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("render_report={}", agent.host_port()),
        ),
    ]);
    if let Some(tools) = tools_yaml {
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "upstream-meta-{}.yaml",
            triton_tests::free_tcp_port()
        ));
        std::fs::write(
            &path,
            format!("version: \"0.2\"\nadapters: {{}}\ntools:\n{tools}"),
        )
        .unwrap();
        env.insert(
            "TRITON_MANIFEST_PATH".to_string(),
            path.display().to_string(),
        );
    }
    TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
}

async fn rpc(
    proc: &TritonProcess,
    session: Option<&str>,
    method: &str,
    params: Value,
) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("http://{}/", proc.mcp_addr))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(s) = session {
        req = req.header("mcp-session-id", s);
    }
    req.send().await.expect("POST mcp")
}

/// A plain MCP client (no MCP Apps).
async fn call(proc: &TritonProcess) -> Value {
    call_as(proc, None).await
}

/// `session`: a client that declared MCP Apps in `initialize`.
async fn call_as(proc: &TritonProcess, session: Option<&str>) -> Value {
    rpc(
        proc,
        session,
        "tools/call",
        json!({ "name": "render_report", "arguments": { "report_id": "revenue" } }),
    )
    .await
    .json()
    .await
    .expect("decode mcp")
}

/// A session of a client that declares MCP Apps.
async fn card_session(proc: &TritonProcess) -> String {
    let resp = rpc(
        proc,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "extensions": { UI: {
                "mimeTypes": [triton_core::a2ui::mcp_app::MIME_TYPE] } } },
            "clientInfo": { "name": "meta-test", "version": "1" }
        }),
    )
    .await;
    resp.headers()
        .get("mcp-session-id")
        .expect("a session")
        .to_str()
        .unwrap()
        .to_string()
}

fn chart() -> Value {
    json!({ "kind": "bar", "x": "region", "y": "revenue",
            "rows": [ { "region": "North", "revenue": 10 } ] })
}

fn full_meta() -> Value {
    json!({
        "content": [ { "type": "text", "text": "Revenue by region" } ],
        "structuredContent": { "facts": [] },
        "_meta": {
            "ui": { "resourceUri": "ui://render_report/revenue" },
            "tool_trace": [ { "tool": "query" } ],
            "png_base64": "iVBORw0KGgo=",
            "svgs": [ "<svg/>" ],
            "chart": chart(),
            "vega_specs": [ { "mark": "bar" } ],
            // Not on the allow-list: dropped.
            "openai/widgetAccessible": true,
            "custom": { "rows": [1, 2, 3] },
            // Protected: an upstream must not set any of these.
            "trace_id": "forged-trace",
            "triton/a2ui": [ { "forged": true } ],
            "triton/in_card": "forged-token",
            "triton/future": 1,
            "io.modelcontextprotocol/related-task": { "taskId": "upstream-task" },
            "tasks.mcp.example/hint": 1
        }
    })
}

const CHART_KEYS: [&str; 4] = ["chart", "png_base64", "svgs", "vega_specs"];
const DROPPED: [&str; 8] = [
    "openai/widgetAccessible",
    "custom",
    "triton/a2ui",
    "triton/in_card",
    "triton/future",
    "io.modelcontextprotocol/related-task",
    "tasks.mcp.example/hint",
    "rows",
];

fn assert_tritons_own(meta: &Value) {
    let trace = meta["trace_id"].as_str().expect("trace_id");
    assert_ne!(trace, "forged-trace", "{meta}");
    assert!(!trace.is_empty());
}

/// A client that declared MCP Apps gets the chart data next to `ui` and
/// `tool_trace`; nothing off the list, nothing Triton owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_host_gets_the_allowed_keys_and_nothing_else() {
    let agent = FakeAgent::start_returning(full_meta()).await;
    let proc = spawn_for(&agent).await;
    let session = card_session(&proc).await;
    let mcp = call_as(&proc, Some(&session)).await;
    let meta = &mcp["result"]["_meta"];
    assert_eq!(meta["png_base64"], "iVBORw0KGgo=", "{mcp}");
    assert_eq!(meta["svgs"], json!(["<svg/>"]));
    assert_eq!(meta["chart"], chart());
    assert_eq!(meta["vega_specs"][0]["mark"], "bar");
    assert_eq!(meta["ui"]["resourceUri"], "ui://render_report/revenue");
    assert_eq!(meta["tool_trace"][0]["tool"], "query");
    assert_tritons_own(meta);
    for k in DROPPED {
        assert!(meta.get(k).is_none(), "{k} passed: {meta}");
    }
    assert_eq!(mcp["result"]["isError"], false);
}

/// A client that renders no card gets no chart data (#379: card data goes
/// to card hosts only), and still nothing off the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_client_gets_no_chart_data_and_no_unknown_keys() {
    let agent = FakeAgent::start_returning(full_meta()).await;
    let proc = spawn_for(&agent).await;
    let mcp = call(&proc).await;
    let meta = &mcp["result"]["_meta"];
    for k in CHART_KEYS.iter().chain(DROPPED.iter()) {
        assert!(meta.get(*k).is_none(), "{k} reached a plain client: {meta}");
    }
    // `ui` (the host decides whether to render it) and the debug trace go
    // to every client, as before.
    assert_eq!(
        meta["ui"]["resourceUri"], "ui://render_report/revenue",
        "{meta}"
    );
    assert_eq!(meta["tool_trace"][0]["tool"], "query");
    assert_tritons_own(meta);
}

/// The operator adds keys per tool and widens or switches off a built-in
/// one (`tools.<name>.meta`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_extends_the_list_per_tool() {
    let agent = FakeAgent::start_returning(full_meta()).await;
    let proc = spawn_with(
        &agent,
        Some(
            "  render_report:\n    meta:\n      custom: all\n      png_base64: all\n      \
             vega_specs: \"off\"\n",
        ),
    )
    .await;
    let plain = call(&proc).await;
    let meta = &plain["result"]["_meta"];
    assert_eq!(meta["custom"]["rows"], json!([1, 2, 3]), "{meta}");
    assert_eq!(meta["png_base64"], "iVBORw0KGgo=");
    assert!(meta.get("chart").is_none(), "{meta}");
    let session = card_session(&proc).await;
    let host = call_as(&proc, Some(&session)).await;
    assert!(
        host["result"]["_meta"].get("vega_specs").is_none(),
        "{host}"
    );
    assert_eq!(host["result"]["_meta"]["chart"], chart());
}

/// A Triton-owned or MCP-reserved key cannot be allowed: such a manifest
/// does not boot.
#[test]
fn a_reserved_key_cannot_be_allowed() {
    for key in ["trace_id", "triton/a2ui", "io.modelcontextprotocol/x"] {
        let yaml = format!(
            "version: \"0.2\"\nadapters: {{}}\ntools:\n  t:\n    meta:\n      \"{key}\": all\n"
        );
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "reserved-meta-{}.yaml",
            triton_tests::free_tcp_port()
        ));
        std::fs::write(&path, yaml).unwrap();
        let m = triton_manifest::Manifest::load(&path).expect("parses");
        assert!(
            m.validate(triton_manifest::Env::Dev).is_err(),
            "{key} was accepted"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_meta_beyond_the_size_budget_is_dropped_key_by_key() {
    // 5 MiB of base64 exceeds the 4 MiB budget on its own; the small keys
    // around it still pass.
    let big = "A".repeat(5 * 1024 * 1024);
    let agent = FakeAgent::start_returning(json!({
        "content": [ { "type": "text", "text": "big" } ],
        "_meta": { "svgs": [ "<svg/>" ], "png_base64": big, "chart": chart() }
    }))
    .await;
    let proc = spawn_for(&agent).await;
    let session = card_session(&proc).await;
    let mcp = call_as(&proc, Some(&session)).await;
    let meta = &mcp["result"]["_meta"];
    assert!(meta.get("png_base64").is_none(), "over-budget key dropped");
    assert_eq!(meta["svgs"], json!(["<svg/>"]));
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

/// The upstream's `_meta` is for the host and its app, never the model:
/// its allowed keys ride the result `_meta`, and none of it stays in
/// `structuredContent` (which ChatGPT shows to its model), for a card host
/// and for a plain client alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_meta_never_rides_the_structured_content() {
    let agent = FakeAgent::start_returning(full_meta()).await;
    let proc = spawn_for(&agent).await;
    let session = card_session(&proc).await;
    for mcp in [call(&proc).await, call_as(&proc, Some(&session)).await] {
        let structured = mcp["result"]["structuredContent"].to_string();
        for needle in [
            "iVBORw0KGgo=",
            "ui://render_report/revenue",
            "\"custom\"",
            "tool_trace",
        ] {
            assert!(
                !structured.contains(needle),
                "{needle} in structuredContent: {structured}"
            );
        }
        assert_eq!(mcp["result"]["_meta"]["tool_trace"][0]["tool"], "query");
    }
}
