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
