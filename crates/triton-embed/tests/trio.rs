//! triton-embed serves the REST/MCP/A2A trio for an in-process tool on a
//! single port. No mocks: a real `axum::serve` over a real TCP socket,
//! driven by a real `reqwest` client. No Consul/Vault — the embed host
//! *is* the trio.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};

/// Trivial in-process tool: echoes its args back.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        Ok(json!({ "echoed": args }))
    }
}

async fn boot() -> String {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), "test"));
    let app = router(dispatcher, &EmbedOpts::dev());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trio_on_one_port() {
    let base = boot().await;
    let http = reqwest::Client::new();
    let marker = "trio-marker-42";

    // REST at the root.
    let rest: Value = http
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": marker }))
        .send()
        .await
        .expect("REST")
        .json()
        .await
        .expect("json");
    assert_eq!(rest["result"]["echoed"]["marker"], marker, "REST echo");
    assert!(
        rest["trace_id"].as_str().is_some(),
        "REST carries trace_id: {rest}"
    );

    // MCP nested at /mcp.
    let mcp: Value = http
        .post(format!("{base}/mcp"))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "echo", "arguments": { "marker": marker } }
        }))
        .send()
        .await
        .expect("MCP")
        .json()
        .await
        .expect("json");
    assert_eq!(
        mcp["result"]["structuredContent"]["result"]["echoed"]["marker"], marker,
        "MCP echo: {mcp}"
    );
    assert!(
        mcp["result"]["_meta"]["trace_id"].as_str().is_some(),
        "MCP carries trace_id: {mcp}"
    );

    // A2A nested at /a2a.
    let a2a: Value = http
        .post(format!("{base}/a2a/message:send"))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "echo", "args": { "marker": marker } } }]
        }))
        .send()
        .await
        .expect("A2A")
        .json()
        .await
        .expect("json");
    assert_eq!(
        a2a["parts"][0]["data"]["result"]["echoed"]["marker"], marker,
        "A2A echo: {a2a}"
    );
    assert!(
        a2a["metadata"]["trace_id"].as_str().is_some(),
        "A2A carries trace_id: {a2a}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_and_tools_at_root() {
    let base = boot().await;
    let http = reqwest::Client::new();

    let health = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    let tools: Value = http
        .get(format!("{base}/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "echo"),
        "echo listed: {tools}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_endpoint_correlates_one_call() {
    let base = boot().await;
    let http = reqwest::Client::new();

    // One call → grab its trace_id.
    let rest: Value = http
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": "trace-me" }))
        .send()
        .await
        .expect("REST")
        .json()
        .await
        .expect("json");
    let trace_id = rest["trace_id"].as_str().expect("trace_id").to_string();

    // The Trace timeline for that id.
    let trace: Value = http
        .get(format!("{base}/v1/trace/{trace_id}"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("trace")
        .json()
        .await
        .expect("json");
    assert_eq!(trace["trace_id"], trace_id);
    let entries = trace["entries"].as_array().expect("entries");
    assert!(
        entries
            .iter()
            .any(|e| e["trace_id"] == trace_id && e["protocol"] == "rest"),
        "timeline has the rest dispatch: {trace}"
    );

    // With the `capture` feature, the request/response bodies are present
    // and the captured request carries our marker (and never a token).
    #[cfg(feature = "capture")]
    {
        let bodies = trace["bodies"].as_array().expect("bodies");
        assert!(!bodies.is_empty(), "capture on → bodies present: {trace}");
        let blob = trace.to_string();
        assert!(blob.contains("trace-me"), "captured request body: {trace}");
        assert!(
            !blob.to_lowercase().contains("dev-token"),
            "captured bodies must not contain the bearer: {trace}"
        );
    }
}

/// TS-04: the dev-only `capture` middleware must NOT buffer a streaming
/// (SSE) response — `to_bytes` would hold the whole body until the stream
/// closes, breaking incremental delivery. We drive an upstream whose
/// stream emits one frame then never terminates: if capture buffered, the
/// first frame would never reach the client and this would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_does_not_buffer_an_sse_response() {
    use futures::StreamExt;
    use futures::stream::{self, BoxStream};
    use std::time::Duration;
    use triton_core::{Principal, StreamEvent, UpstreamDispatch};

    /// Upstream whose streaming response yields a `tool` frame then hangs.
    struct SlowStreamUpstream;

    #[async_trait]
    impl UpstreamDispatch for SlowStreamUpstream {
        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _p: &Principal,
        ) -> Result<Value, TritonError> {
            Ok(json!({ "surface": { "components": [] } }))
        }
        async fn invoke_streaming(
            &self,
            _tool: &str,
            _args: Value,
            _p: &Principal,
        ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
            let s = stream::once(async { StreamEvent::Tool(json!({ "step": "search" })) })
                .chain(stream::pending());
            Ok(s.boxed())
        }
        async fn list_agents(&self) -> Vec<String> {
            vec!["slow".to_string()]
        }
    }

    let dispatcher = Arc::new(
        Dispatcher::new(Arc::new(ToolRegistry::new()), "test")
            .with_upstream(Arc::new(SlowStreamUpstream)),
    );
    let app = router(dispatcher, &EmbedOpts::dev());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");

    // Response headers must arrive promptly. If `capture` buffered the
    // (never-closing) stream, `to_bytes` would hold the whole response —
    // even `send()` (which waits for headers) would never return — so we
    // bound it to fail fast rather than hang the CI job on a regression.
    let resp = tokio::time::timeout(
        Duration::from_secs(3),
        reqwest::Client::new()
            .post(format!("{base}/v1/tools/slow"))
            .bearer_auth("dev-token")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&json!({ "q": "hi" }))
            .send(),
    )
    .await
    .expect("response headers must arrive promptly — capture must not buffer SSE")
    .expect("POST streaming");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/event-stream"),
    );

    // The first frame must also arrive promptly (not just the headers).
    let mut bytes = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(3), bytes.next())
        .await
        .expect("first SSE chunk must arrive promptly — capture must not buffer")
        .expect("a chunk")
        .expect("chunk ok");
    assert!(
        String::from_utf8_lossy(&first).contains("event: tool"),
        "expected the first streamed frame, got: {:?}",
        String::from_utf8_lossy(&first)
    );
    // Disconnect the never-ending stream.
    drop(bytes);
}
