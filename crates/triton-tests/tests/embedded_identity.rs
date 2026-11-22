//! The server identity an embedded host configures through the ingress
//! (`TRITON_MCP_SERVER_NAME`, `_TITLE`, `TRITON_MCP_INSTRUCTIONS`,
//! `TRITON_A2UI_AGENT_DISPLAY_NAME`).
//!
//! Its own process: the default A2UI display name is process-wide, so a
//! refused configuration must not install it, and the test must not share
//! the process with others that render surfaces.

use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::EmbedOpts;
use triton_embed::ingress::IngressConfig;

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

fn dispatcher() -> Arc<Dispatcher> {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    Arc::new(Dispatcher::new(
        Arc::new(reg),
        "test".to_string(),
        DispatchControls::unenforced(),
    ))
}

fn ingress(args: &[&str]) -> IngressConfig {
    let mut argv = vec!["triton"];
    argv.extend_from_slice(args);
    IngressConfig::try_parse_from(argv).expect("ingress")
}

fn theme_name() -> Value {
    let card = json!({ "surface": { "components": [
        { "kind": "header", "title": "Two items" },
        { "kind": "button", "label": "More",
          "action": { "type": "send_message", "text": "Show more" } }
    ] } });
    let messages = triton_core::a2ui::ge::build_messages(&card).expect("messages");
    messages[0]["createSurface"]["theme"]["agentDisplayName"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_identity_installs_nothing_and_the_accepted_one_serves() {
    // A valid display name beside an invalid server name: refused, and the
    // display name is not installed.
    let refused = EmbedOpts::dev().ingress(ingress(&[
        "--a2ui-agent-display-name=Rejected Agent",
        "--mcp-server-name=",
    ]));
    let err = triton_embed::router_parts(dispatcher(), &refused)
        .err()
        .expect("refused");
    assert!(err.to_string().contains("TRITON_MCP_SERVER_NAME"), "{err}");
    assert_eq!(theme_name(), "DataZoo Agent");

    let accepted = EmbedOpts::dev().ingress(ingress(&[
        "--a2ui-agent-display-name=Portfolio Agent",
        "--mcp-server-name=portfolio",
        "--mcp-server-title=Portfolio",
        "--mcp-instructions=Use echo to repeat a message.",
    ]));
    let parts = triton_embed::router_parts(dispatcher(), &accepted).expect("accepted");
    assert_eq!(theme_name(), "Portfolio Agent");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, parts.router).await;
    });
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" } }))
        .send()
        .await
        .expect("POST /mcp")
        .json()
        .await
        .expect("json");
    let result = &body["result"];
    assert_eq!(result["serverInfo"]["name"], "portfolio", "{body}");
    assert_eq!(result["serverInfo"]["title"], "Portfolio", "{body}");
    assert_eq!(
        result["instructions"], "Use echo to repeat a message.",
        "{body}"
    );
}
