//! An upstream that brings its own MCP App declares it on its tool
//! descriptors: `_meta.ui.resourceUri` names the app, `_meta.ui.visibility`
//! says who may call the tool (MCP Apps 2026-01-26, "Resource Discovery").
//!
//! Triton's `tools/list` writes its answer-card widget into the descriptor
//! of every upstream tool that answers with a surface. An upstream's own
//! `_meta.ui` wins: its descriptor carries the upstream's `ui` object and
//! its `openai/*` aliases verbatim, never the widget, and no other upstream
//! descriptor key. A tool without its own `_meta.ui` still gets the widget.
//! The app's `ui://<authority>/...` resource is read from its owner, and an
//! app-only tool (`visibility: ["app"]`) stays listed (the host hides it
//! from its model) and callable.
//!
//! An embedding host has no `adapter.yaml` tool section: it lists the result
//! `_meta` keys its app needs per tool in `EmbedOpts::upstream_meta`, with
//! the audiences of `tools.<name>.meta`.
//!
//! Real binary or the embedded router, real TCP, real upstreams. No mocks.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::{
    DispatchContext, Dispatcher, Principal, ToolRegistry, UpstreamDispatch, UpstreamToolInfo,
};
use triton_embed::{EmbedOpts, MetaAudience, router_parts};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

const UI: &str = "io.modelcontextprotocol/ui";
const APP: &str = "ui://peacock/views";

/// The upstream's `tools/list`: a tool with its own app, an app-only tool
/// of that app, and a tool without an app of its own.
fn upstream_tools() -> Value {
    json!({ "tools": [
        { "name": "render_report", "description": "A report as an interactive view.",
          "annotations": { "readOnlyHint": true },
          "inputSchema": { "type": "object" },
          "_meta": {
              "ui": { "resourceUri": APP },
              "openai/outputTemplate": APP,
              "x-private": "never listed"
          } },
        { "name": "report_refresh", "description": "Refreshes the report view.",
          "inputSchema": { "type": "object" },
          "_meta": {
              "ui": { "resourceUri": APP, "visibility": ["app"] },
              "openai/visibility": "private",
              "openai/widgetAccessible": true
          } },
        { "name": "plain_report", "description": "A report without an app.",
          "inputSchema": { "type": "object" } }
    ] })
}

async fn spawn_for(agent: &FakeAgent) -> TritonProcess {
    let ep = agent.host_port();
    TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_report={ep},report_refresh={ep},plain_report={ep},peacock={ep}"),
            ),
        ]),
    )
    .await
}

async fn rpc(url: &str, session: Option<&str>, method: &str, params: Value) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(s) = session {
        req = req.header("mcp-session-id", s);
    }
    req.send().await.expect("POST mcp")
}

async fn rpc_json(url: &str, session: Option<&str>, method: &str, params: Value) -> Value {
    rpc(url, session, method, params)
        .await
        .json()
        .await
        .expect("decode mcp")
}

/// A session of a client that declares MCP Apps.
async fn card_session(url: &str) -> String {
    let resp = rpc(
        url,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "extensions": { UI: {
                "mimeTypes": [triton_core::a2ui::mcp_app::MIME_TYPE] } } },
            "clientInfo": { "name": "app-test", "version": "1" }
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

fn tool<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("{name} not listed: {list}"))
}

fn widget_uri() -> String {
    triton_core::a2ui::mcp_app::resource_uri().to_string()
}

/// What the descriptors of the three tools must say, on any host.
fn assert_descriptors(list: &Value) {
    let own = tool(list, "render_report");
    assert_eq!(
        own["_meta"],
        json!({ "ui": { "resourceUri": APP }, "openai/outputTemplate": APP }),
        "an upstream's own app wins over the widget, and only `ui` and `openai/*` pass: {own}"
    );
    let app_only = tool(list, "report_refresh");
    assert_eq!(
        app_only["_meta"],
        json!({
            "ui": { "resourceUri": APP, "visibility": ["app"] },
            "openai/visibility": "private",
            "openai/widgetAccessible": true
        }),
        "an app-only tool stays listed with its visibility: {app_only}"
    );
    let plain = tool(list, "plain_report");
    assert_eq!(
        plain["_meta"]["ui"]["resourceUri"],
        json!(widget_uri()),
        "a tool without its own app still gets the answer-card widget: {plain}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upstreams_own_app_wins_on_its_tool_descriptors() {
    let agent = FakeAgent::start_mcp_apps().await;
    agent.set_tools_list(upstream_tools());
    let proc = spawn_for(&agent).await;
    let url = format!("http://{}/", proc.mcp_addr);
    let session = card_session(&url).await;

    let list = rpc_json(&url, Some(&session), "tools/list", json!({})).await;
    assert_descriptors(&list);

    // The app's resource is read from its owner.
    let read = rpc_json(
        &url,
        Some(&session),
        "resources/read",
        json!({ "uri": APP }),
    )
    .await;
    assert_eq!(read["result"]["contents"][0]["uri"], APP, "{read}");
    // And listed once, by the URI its descriptors name, with the MCP Apps
    // mime type, beside Triton's own widget: a host that prefetches or
    // reviews resources at connection time finds it.
    let listed = rpc_json(&url, Some(&session), "resources/list", json!({})).await;
    let apps: Vec<&Value> = listed["result"]["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter(|r| r["uri"] == APP)
        .collect();
    assert_eq!(apps.len(), 1, "{listed}");
    assert_eq!(apps[0]["mimeType"], "text/html;profile=mcp-app", "{listed}");
    assert!(
        listed["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "answer-card"),
        "{listed}"
    );

    // The app-only tool is callable (the host, not Triton, keeps it from
    // its model), and its result brings the app's link, not the card.
    let call = rpc_json(
        &url,
        Some(&session),
        "tools/call",
        json!({ "name": "report_refresh", "arguments": {} }),
    )
    .await;
    assert_eq!(call["result"]["isError"], false, "{call}");
    assert_eq!(
        call["result"]["_meta"]["ui"]["resourceUri"],
        "ui://peacock/r1"
    );
    assert!(
        call["result"]["_meta"].get("triton/a2ui").is_none(),
        "{call}"
    );
}

// ── an embedding host ─────────────────────────────────────────────────

struct InProcess;

#[async_trait]
impl UpstreamDispatch for InProcess {
    async fn invoke(
        &self,
        tool: &str,
        _args: Value,
        _p: &Principal,
        _ctx: &DispatchContext,
    ) -> Result<Value, TritonError> {
        match tool {
            "render_report" | "report_refresh" => Ok(json!({
                "surface": { "text": "Revenue report" },
                "_meta": {
                    "ui": { "resourceUri": APP },
                    "peacock/view": [{ "version": "v0.9" }],
                    "peacock/other": "not listed"
                }
            })),
            "plain_report" => Ok(json!({ "surface": { "text": "Plain" } })),
            other => Err(TritonError::Validation(format!("unknown tool: {other}"))),
        }
    }

    async fn read_resource(
        &self,
        uri: &str,
        _p: &Principal,
        _ctx: &DispatchContext,
    ) -> Result<Value, TritonError> {
        Ok(
            json!({ "contents": [{ "uri": uri, "mimeType": "text/html;profile=mcp-app",
            "text": "<!doctype html><p>app</p>" }] }),
        )
    }

    async fn list_agents(&self) -> Vec<String> {
        vec![
            "plain_report".into(),
            "render_report".into(),
            "report_refresh".into(),
        ]
    }

    async fn agent_tools(&self) -> HashMap<String, UpstreamToolInfo> {
        upstream_tools()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().to_string(),
                    UpstreamToolInfo {
                        input_schema: t["inputSchema"].clone(),
                        description: t["description"].as_str().map(String::from),
                        annotations: t.get("annotations").cloned(),
                        meta: t.get("_meta").cloned(),
                    },
                )
            })
            .collect()
    }
}

async fn embedded() -> String {
    let dispatcher = Arc::new(
        Dispatcher::new(
            Arc::new(ToolRegistry::new()),
            "local".to_string(),
            DispatchControls::unenforced(),
        )
        .with_surfaces(triton_embed::surfaces::registry(None).expect("registry"))
        .with_upstream(Arc::new(InProcess)),
    );
    let keys = BTreeMap::from([("peacock/view".to_string(), MetaAudience::CardHost)]);
    let opts = EmbedOpts::dev().env("local").upstream_meta(BTreeMap::from([
        ("render_report".to_string(), keys.clone()),
        ("report_refresh".to_string(), keys),
    ]));
    let parts = router_parts(dispatcher, &opts).expect("embedded router");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, parts.router).await;
    });
    format!("http://{addr}/mcp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedding_host_lists_its_apps_result_keys_per_tool() {
    let url = embedded().await;
    let session = card_session(&url).await;

    let list = rpc_json(&url, Some(&session), "tools/list", json!({})).await;
    assert_descriptors(&list);

    let read = rpc_json(
        &url,
        Some(&session),
        "resources/read",
        json!({ "uri": APP }),
    )
    .await;
    assert_eq!(
        read["result"]["contents"][0]["mimeType"], "text/html;profile=mcp-app",
        "{read}"
    );

    // A card host gets the listed key, never an unlisted one, and no card.
    let call = rpc_json(
        &url,
        Some(&session),
        "tools/call",
        json!({ "name": "render_report", "arguments": {} }),
    )
    .await;
    let meta = &call["result"]["_meta"];
    assert_eq!(meta["ui"]["resourceUri"], APP, "{call}");
    assert_eq!(
        meta["peacock/view"],
        json!([{ "version": "v0.9" }]),
        "{call}"
    );
    assert!(meta.get("peacock/other").is_none(), "{call}");
    assert!(meta.get("triton/a2ui").is_none(), "{call}");

    // A plain client does not get the app's data.
    let plain = rpc_json(
        &url,
        None,
        "tools/call",
        json!({ "name": "render_report", "arguments": {} }),
    )
    .await;
    assert!(
        plain["result"]["_meta"].get("peacock/view").is_none(),
        "{plain}"
    );
    assert_eq!(
        plain["result"]["_meta"]["ui"]["resourceUri"], APP,
        "{plain}"
    );
}
