//! Streaming (SSE) response path — issue #132.
//!
//! No-mock integration tests: a real spawned `triton`, a real upstream
//! `FakeAgent` over TCP, and a real `reqwest` client. The buffered JSON
//! path must stay byte-identical for callers that don't negotiate SSE.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

/// Audit lines on stdout for `tool` in the `dispatch` phase. Each test
/// spawns its own `triton`, so the stdout buffer is isolated.
fn dispatch_lines_for(proc: &TritonProcess, tool: &str) -> Vec<String> {
    proc.stdout_snapshot()
        .into_iter()
        .filter(|l| {
            l.contains("\"phase\":\"dispatch\"") && l.contains(&format!("\"tool\":\"{tool}\""))
        })
        .collect()
}

/// Poll up to `deadline` for exactly one dispatch audit line, then
/// return it. Panics with the captured stdout if the count is wrong.
async fn await_single_dispatch(proc: &TritonProcess, tool: &str) -> String {
    let start = Instant::now();
    loop {
        let lines = dispatch_lines_for(proc, tool);
        if lines.len() == 1 {
            return lines.into_iter().next().unwrap();
        }
        assert!(
            lines.len() <= 1,
            "expected exactly one dispatch audit line for {tool}, got {}:\n{:#?}",
            lines.len(),
            lines
        );
        if start.elapsed() > Duration::from_secs(2) {
            panic!(
                "no dispatch audit line for {tool} within 2s; stdout:\n{:#?}",
                proc.stdout_snapshot()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A caller that negotiates `Accept: text/event-stream` against a plain
/// (non-streaming) upstream still gets a valid SSE response whose single
/// terminal `done` frame carries the same payload the buffered path
/// returns — and the one ADR-6 dispatch audit line still fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_over_buffered_upstream_emits_single_done_frame() {
    let agent = FakeAgent::start_echoing().await;
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([(
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("hello={}", agent.host_port()),
        )]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/hello"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/hello");

    assert_eq!(resp.status(), 200, "streaming call should be 200 OK");
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "expected an SSE content-type, got {ct:?}"
    );

    let body = resp.text().await.expect("read SSE body");
    assert!(
        body.contains("event: done"),
        "expected a terminal done frame, body was:\n{body}"
    );
    assert!(
        body.contains("\"echoed\""),
        "done frame should carry the echoed upstream payload, body was:\n{body}"
    );
    assert!(
        !body.contains("event: error"),
        "clean dispatch must not emit an error frame, body was:\n{body}"
    );

    // ADR-6: exactly one dispatch audit line, result ok.
    let line = await_single_dispatch(&triton, "hello").await;
    assert!(
        line.contains("\"result\":\"ok\""),
        "dispatch audit should be ok, was:\n{line}"
    );
}

/// The default (no Accept negotiation) path is unchanged: a plain JSON
/// envelope, not SSE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_without_accept_stays_buffered_json() {
    let agent = FakeAgent::start_echoing().await;
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([(
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("hello={}", agent.host_port()),
        )]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/hello"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/hello");

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "buffered path must stay JSON, got {ct:?}"
    );
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["result"]["echoed"]["q"], "hi");
}

/// FR-A-6 regression: MCP is JSON-RPC over Streamable HTTP with plain
/// JSON responses; SSE is explicitly not required. Even when a caller
/// sends `Accept: text/event-stream`, the MCP surface must answer with
/// JSON, never an event stream. Only the REST/A2A web surfaces stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_ignores_event_stream_accept_and_stays_json() {
    let triton = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let resp = reqwest::Client::new()
        .post(triton.mcp_url("/"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("POST / tools/list");

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        !ct.starts_with("text/event-stream"),
        "MCP must not stream (FR-A-6), got content-type {ct:?}"
    );
    let body: serde_json::Value = resp.json().await.expect("json-rpc body");
    assert_eq!(body["jsonrpc"], "2.0");
}
