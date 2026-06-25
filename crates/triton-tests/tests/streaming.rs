//! Streaming (SSE) response path — issue #132.
//!
//! No-mock integration tests: a real spawned `triton`, a real upstream
//! `FakeAgent` over TCP, and a real `reqwest` client. The buffered JSON
//! path must stay byte-identical for callers that don't negotiate SSE.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::StreamExt;
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

/// Spawn a Triton whose `tool` routes to `agent`.
async fn triton_with_upstream(tool: &str, agent: &FakeAgent) -> TritonProcess {
    TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([(
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("{tool}={}", agent.host_port()),
        )]),
    )
    .await
}

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

/// A genuinely streaming upstream's `tool`/`token`/`done` frames are
/// relayed incrementally to the client (TTFB measurably precedes the
/// terminal frame), and the single ADR-6 audit line fires at termination
/// with `result: ok` and a `ttfb_ms`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_streaming_upstream_relays_incremental_frames() {
    let agent = FakeAgent::start_streaming().await;
    let triton = triton_with_upstream("grounded", &agent).await;

    let start = Instant::now();
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/grounded"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST streaming");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/event-stream")
    );

    let mut stream = resp.bytes_stream();
    let mut first_at: Option<Duration> = None;
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.expect("stream chunk");
        if first_at.is_none() {
            first_at = Some(start.elapsed());
        }
        buf.push_str(&String::from_utf8_lossy(&bytes));
    }
    let total = start.elapsed();
    let ttfb = first_at.expect("at least one chunk");

    assert!(buf.contains("event: tool"), "missing tool frame:\n{buf}");
    assert!(buf.contains("event: token"), "missing token frame:\n{buf}");
    assert!(buf.contains("event: done"), "missing done frame:\n{buf}");
    assert!(
        buf.contains("\"echoed\""),
        "done frame should echo the body:\n{buf}"
    );
    // Incremental: the first byte arrives well before the stream closes
    // (the fake agent spaces frames ~15ms apart).
    assert!(
        total > ttfb + Duration::from_millis(20),
        "frames should arrive over time; ttfb {ttfb:?} total {total:?}"
    );

    let line = await_single_dispatch(&triton, "grounded").await;
    assert!(
        line.contains("\"result\":\"ok\""),
        "clean stream should audit ok:\n{line}"
    );
    assert!(
        line.contains("\"ttfb_ms\""),
        "streamed dispatch should record ttfb_ms:\n{line}"
    );
}

/// An upstream that closes the stream before sending a terminal `done`
/// is audited once as an error with the `upstream_truncated` detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_upstream_truncation_audits_error_once() {
    let agent = FakeAgent::start_streaming_truncated().await;
    let triton = triton_with_upstream("grounded", &agent).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/grounded"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST streaming");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("event: token"), "expected progress frames");
    assert!(
        !body.contains("event: done"),
        "truncated upstream must not produce a done frame:\n{body}"
    );

    let line = await_single_dispatch(&triton, "grounded").await;
    assert!(
        line.contains("\"result\":\"error:tool\""),
        "truncation should audit as a tool error:\n{line}"
    );
    assert!(
        line.contains("\"status_detail\":\"upstream_truncated\""),
        "truncation should carry the upstream_truncated detail:\n{line}"
    );
}

/// When the client disconnects mid-stream, the dispatcher still emits
/// exactly one audit line — distinctly marked as a client disconnect,
/// not a server error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_client_disconnect_audits_distinctly() {
    let agent = FakeAgent::start_streaming().await;
    let triton = triton_with_upstream("grounded", &agent).await;

    {
        let resp = reqwest::Client::new()
            .post(triton.rest_url("/v1/tools/grounded"))
            .bearer_auth("dev-token")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&serde_json::json!({ "q": "hi" }))
            .send()
            .await
            .expect("POST streaming");
        assert_eq!(resp.status(), 200);
        // Read only the first chunk (the first frame), then drop the
        // response — the remaining frames are still ~15ms apart, so the
        // upstream stream is mid-flight when we bail.
        let mut stream = resp.bytes_stream();
        let _first = stream.next().await.expect("first chunk").expect("ok");
        // `stream` (and the underlying connection) dropped here.
    }

    // Poll for the single disconnect-marked audit line.
    let start = Instant::now();
    let line = loop {
        let lines = dispatch_lines_for(&triton, "grounded");
        if let Some(l) = lines.first() {
            assert_eq!(lines.len(), 1, "exactly one audit line, got:\n{lines:#?}");
            break l.clone();
        }
        if start.elapsed() > Duration::from_secs(3) {
            panic!(
                "no disconnect audit line within 3s; stdout:\n{:#?}",
                triton.stdout_snapshot()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        line.contains("\"status_detail\":\"client_disconnect\""),
        "client disconnect should be marked distinctly:\n{line}"
    );
}

/// When the caller negotiates BOTH SSE and an A2UI version, the terminal
/// `done` frame carries the built A2UI envelope (not the raw surface),
/// while `tool`/`token` progress frames pass through untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_with_a2ui_wraps_terminal_done_in_envelope() {
    let agent = FakeAgent::start_streaming_surface().await;
    let triton = triton_with_upstream("grounded", &agent).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/grounded"))
        .bearer_auth("dev-token")
        // One Accept header negotiating both the stream and A2UI 0.9.
        .header(
            reqwest::header::ACCEPT,
            "text/event-stream, application/json+a2ui;version=0.9",
        )
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST streaming + a2ui");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");

    // Progress frames pass through.
    assert!(body.contains("event: tool"), "missing tool frame:\n{body}");
    assert!(
        body.contains("event: token"),
        "missing token frame:\n{body}"
    );
    // Terminal done carries the v0.9 envelope, not the raw surface.
    assert!(body.contains("event: done"), "missing done frame:\n{body}");
    assert!(
        body.contains("\"version\":\"0.9\""),
        "done frame should be the built A2UI 0.9 envelope:\n{body}"
    );

    let line = await_single_dispatch(&triton, "grounded").await;
    assert!(
        line.contains("\"result\":\"ok\""),
        "clean A2UI stream should audit ok:\n{line}"
    );
}

/// The A2A surface streams too (issue #132): `POST /message:send` with
/// `Accept: text/event-stream` relays `tool`/`token`/`done` frames, and
/// when `metadata.a2ui_version` is set the terminal `done` carries the
/// built envelope. Exactly one `a2a` dispatch audit line fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_sse_streams_frames_and_wraps_a2ui_done() {
    let agent = FakeAgent::start_streaming_surface().await;
    let triton = triton_with_upstream("grounded", &agent).await;

    let resp = reqwest::Client::new()
        .post(triton.a2a_url("/message:send"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({
            "parts": [ { "data": { "tool": "grounded", "args": { "q": "hi" } } } ],
            "metadata": { "a2ui_version": "v0.9" }
        }))
        .send()
        .await
        .expect("POST /message:send streaming");

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "A2A streaming should be an event stream"
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("event: tool"), "missing tool frame:\n{body}");
    assert!(
        body.contains("event: token"),
        "missing token frame:\n{body}"
    );
    assert!(body.contains("event: done"), "missing done frame:\n{body}");
    assert!(
        body.contains("\"version\":\"0.9\""),
        "A2A done should carry the v0.9 envelope:\n{body}"
    );

    let line = await_single_dispatch(&triton, "grounded").await;
    assert!(
        line.contains("\"protocol\":\"a2a\""),
        "audit should record the a2a protocol:\n{line}"
    );
    assert!(
        line.contains("\"result\":\"ok\""),
        "clean A2A stream should audit ok:\n{line}"
    );
}

/// A2A pre-first-byte failure (open breaker / unreachable upstream) when
/// SSE was negotiated still answers with the ordinary A2A error Message,
/// not a half-open event stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_sse_pre_first_byte_error_is_plain_message() {
    // No upstream registered for this tool → unknown-tool validation.
    let triton = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let resp = reqwest::Client::new()
        .post(triton.a2a_url("/message:send"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({
            "parts": [ { "data": { "tool": "nope", "args": {} } } ]
        }))
        .send()
        .await
        .expect("POST /message:send");

    // Unknown tool with no upstream is a validation error (400), and the
    // body is the ordinary A2A error JSON — never an SSE stream.
    assert_eq!(resp.status(), 400);
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        !ct.starts_with("text/event-stream"),
        "pre-first-byte error must not open an event stream, got {ct:?}"
    );
    let body: serde_json::Value = resp.json().await.expect("json error");
    assert_eq!(body["error"], "validation");
}

/// TS-03: a hung upstream (opens the SSE response, sends progress frames,
/// then never sends another byte and never closes) must be cut by the
/// per-frame idle timeout — the client gets a terminal `error` frame and
/// the stream ends, with exactly one dispatch audit line `error:tool`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_sse_idle_timeout_cuts_a_hung_upstream() {
    let agent = FakeAgent::start_streaming_hang().await;
    let triton = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            (
                "TRITON_STATIC_UPSTREAMS".into(),
                format!("grounded={}", agent.host_port()),
            ),
            ("TRITON_STREAM_IDLE_TIMEOUT_MS".into(), "300".into()),
        ]),
    )
    .await;

    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/grounded"))
        .bearer_auth("dev-token")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&serde_json::json!({ "q": "hi" }))
        .send()
        .await
        .expect("POST streaming");
    assert_eq!(resp.status(), 200);

    // The hung agent never closes; only Triton's idle timeout ends the
    // stream. `text()` therefore returns once that terminal error flushes.
    let body = resp.text().await.expect("read SSE body");
    let elapsed = started.elapsed();

    assert!(
        body.contains("event: token"),
        "expected progress frames:\n{body}"
    );
    assert!(
        !body.contains("event: done"),
        "a hung upstream must not produce a done frame:\n{body}"
    );
    assert!(
        body.contains("event: error") && body.contains("idle timeout"),
        "expected a terminal idle-timeout error frame:\n{body}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "idle timeout (300ms) should end the stream well before any total timeout; took {elapsed:?}"
    );

    let line = await_single_dispatch(&triton, "grounded").await;
    assert!(
        line.contains("\"result\":\"error:tool\""),
        "idle timeout should audit as a tool error:\n{line}"
    );
}
