//! An upstream that does not serve the tool Triton routes to it (it
//! answers `404`), and a tool name no upstream serves. Neither may reach a
//! user as a raw error — no `404`, no `Not Found`, no upstream text. No
//! mocks: the real binary against a FakeAgent that answers `404`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

fn env(agent: &FakeAgent) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ghost={}", agent.host_port()),
        ),
        (
            "TRITON_A2A_PUBLIC_URL".to_string(),
            "https://agents.example.test/a2a".to_string(),
        ),
        ("TRITON_A2A_DEFAULT_TOOL".to_string(), "ghost".to_string()),
    ])
}

fn raw(s: &str) -> bool {
    s.contains("404") || s.contains("Not Found") || s.contains("upstream ghost")
}

async fn mcp_call(t: &TritonProcess, tool: &str) -> Value {
    reqwest::Client::new()
        .post(t.rest_url("/mcp"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": { "message": "hi" } } }))
        .send()
        .await
        .expect("tools/call")
        .json()
        .await
        .expect("json")
}

async fn a2a(t: &TritonProcess, version: Option<&str>, method: &str, params: Value) -> String {
    let mut req = reqwest::Client::new()
        .post(t.rest_url("/a2a"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    req.send().await.expect("a2a").text().await.expect("body")
}

/// MCP: an upstream fault is reported without the upstream's status or
/// text — a model relays what it gets to the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_reports_an_upstream_404_without_the_raw_error() {
    let agent = FakeAgent::start_not_found().await;
    let t = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&agent)).await;
    let v = mcp_call(&t, "ghost").await;
    let text = v.to_string();
    assert!(
        !raw(&text),
        "raw upstream error reached the MCP client: {text}"
    );
    assert!(
        v.get("error").is_some() || v["result"]["isError"] == json!(true),
        "still a failure: {v}"
    );
}

/// MCP: a tool name nothing serves is the client's own fault — the
/// spec's `-32602` naming the tool, not a provider error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_answers_an_unknown_tool_as_invalid_params() {
    let agent = FakeAgent::start_not_found().await;
    let t = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&agent)).await;
    let v = mcp_call(&t, "nonexistent").await;
    assert_eq!(v["error"]["code"], json!(-32602), "{v}");
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("nonexistent"), "{v}");
    assert!(!msg.starts_with("tool:"), "not a tool fault: {v}");
}

/// A2A 0.3 and 1.0 (send, stream, get): the failed task's text is the
/// neutral notice, never `upstream ghost returned 404 Not Found`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_reports_an_upstream_404_without_the_raw_error() {
    let agent = FakeAgent::start_not_found().await;
    let t = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&agent)).await;
    let send03 = json!({ "message": { "kind": "message", "role": "user", "messageId": "m1",
        "parts": [{ "kind": "text", "text": "hi" }] } });
    let send10 = |id: &str| json!({ "message": { "role": "ROLE_USER", "messageId": id, "parts": [{ "text": "hi" }] } });
    let body = a2a(&t, None, "message/send", send03.clone()).await;
    assert!(!raw(&body), "0.3 send: {body}");
    let body = a2a(&t, None, "message/stream", send03).await;
    assert!(!raw(&body), "0.3 stream: {body}");
    let body = a2a(&t, Some("1.0"), "SendMessage", send10("m2")).await;
    assert!(!raw(&body), "1.0 send: {body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        v["result"]["task"]["status"]["state"], "TASK_STATE_FAILED",
        "{v}"
    );
    let id = v["result"]["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let body = a2a(&t, Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert!(!raw(&body), "1.0 get: {body}");
    let body = a2a(&t, Some("1.0"), "SendStreamingMessage", send10("m3")).await;
    assert!(!raw(&body), "1.0 stream: {body}");
    // The raw cause stays in the operator's audit line.
    assert!(
        t.stdout_snapshot().iter().any(|l| l.contains("404")),
        "the audit keeps the cause"
    );
}

/// The other side of the line: an upstream's own `error` event is written
/// for the caller (contract §7) and is shown as it is — on MCP and on both
/// A2A streams — while Triton's transport faults are not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upstream_error_event_is_shown_as_written() {
    use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
    const MSG: &str = "The portfolio is locked for maintenance.";
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(300),
        fail_with: Some(MSG.into()),
        ..AsyncPlan::default()
    })
    .await;
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ghost={}", agent.host_port()),
        ),
        ("TRITON_A2A_DEFAULT_TOOL".to_string(), "ghost".to_string()),
    ]);
    env.insert(
        "TRITON_A2A_PUBLIC_URL".to_string(),
        "https://agents.example.test/a2a".to_string(),
    );
    let t = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let v = mcp_call(&t, "ghost").await;
    assert_eq!(v["error"]["message"], json!(MSG), "{v}");
    let body = a2a(
        &t,
        None,
        "message/stream",
        json!({ "message": { "kind": "message", "role": "user", "messageId": "m1",
            "parts": [{ "kind": "text", "text": "hi" }] } }),
    )
    .await;
    assert!(body.contains(MSG), "0.3 stream: {body}");
    let body = a2a(
        &t,
        Some("1.0"),
        "SendStreamingMessage",
        json!({ "message": { "role": "ROLE_USER", "messageId": "m2", "parts": [{ "text": "hi" }] } }),
    )
    .await;
    assert!(body.contains(MSG), "1.0 stream: {body}");
    assert!(!body.contains("tool: "), "no class prefix: {body}");
}
