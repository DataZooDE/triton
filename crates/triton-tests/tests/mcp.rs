//! MCP adapter — hand-rolled JSON-RPC over axum per ADR-2, FR-A-6.
//! Methods: `initialize`, `tools/list`, `tools/call`,
//! `resources/read`. Plain JSON responses, no SSE.
//!
//! No mocks: real binary, real HTTP, real audit lines.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

const RPC_URL_PATH: &str = "/"; // MCP root endpoint

fn mcp_url(proc: &TritonProcess) -> String {
    format!("http://{}{RPC_URL_PATH}", proc.mcp_addr)
}

async fn rpc_request(
    proc: &TritonProcess,
    body: Value,
    bearer: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    let mut req = reqwest::Client::new().post(mcp_url(proc)).json(&body);
    if let Some(b) = bearer {
        req = req.bearer_auth(b);
    }
    let resp = req.send().await.expect("POST mcp");
    let status = resp.status();
    let parsed: Value = resp.json().await.expect("decode mcp body");
    (status, parsed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_returns_server_capabilities() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}, body: {body}");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let server_info = &body["result"]["serverInfo"];
    assert_eq!(server_info["name"], "triton");
    // protocolVersion echoes back per the MCP handshake.
    assert!(body["result"]["protocolVersion"].is_string());
    // Tools capability MUST be advertised since /tools/call works.
    assert!(body["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_returns_echo() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}");
    let tools = body["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools is array: {body}"));
    let echo = tools
        .iter()
        .find(|t| t["name"] == "echo")
        .unwrap_or_else(|| panic!("echo not found: {body}"));
    // MCP's tool descriptor uses `inputSchema` (camelCase), not
    // the REST `input_schema` (snake_case).
    assert_eq!(echo["inputSchema"]["type"], "object");
    assert_eq!(
        echo["inputSchema"]["required"].as_array().unwrap(),
        &vec![json!("message")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_call_echo_round_trips_and_audits() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "echo", "arguments": { "message": "hi via mcp" } }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}, body: {body}");
    // MCP `tools/call` returns `{result: {content: [...], structuredContent: {...}}}`.
    // Triton wraps the structured content under the documented key.
    let inner = &body["result"]["structuredContent"];
    assert_eq!(inner["result"]["echo"], "hi via mcp");
    let trace_id = inner["trace_id"]
        .as_str()
        .expect("trace_id present")
        .to_string();
    let audit = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["trace_id"] == trace_id
    });
    assert_eq!(audit["protocol"], "mcp");
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_way_parity_rest_a2a_mcp() {
    // ACC-1: same input + principal → same inner result across the
    // HTTP trio. Outer envelopes differ; the inner result dict
    // (post-parse) must be byte-equal.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let client = reqwest::Client::new();
    let args = json!({ "message": "three-way parity" });

    let rest: Value = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&args)
        .send()
        .await
        .expect("REST")
        .json()
        .await
        .expect("REST decode");
    let a2a: Value = client
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({ "parts": [{ "data": { "tool": "echo", "args": args } }] }))
        .send()
        .await
        .expect("A2A")
        .json()
        .await
        .expect("A2A decode");
    let (_, mcp) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": { "name": "echo", "arguments": args }
        }),
        Some("dev-token"),
    )
    .await;

    let rest_inner = &rest["result"];
    let a2a_inner = &a2a["parts"][0]["data"]["result"];
    let mcp_inner = &mcp["result"]["structuredContent"]["result"];
    assert_eq!(
        rest_inner, a2a_inner,
        "REST≠A2A: REST={rest_inner} A2A={a2a_inner}"
    );
    assert_eq!(
        rest_inner, mcp_inner,
        "REST≠MCP: REST={rest_inner} MCP={mcp_inner}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_bearer_returns_jsonrpc_auth_error() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "echo", "arguments": { "message": "x" } }
        }),
        None,
    )
    .await;
    // architecture.md §8.3: MCP Auth → -32001. HTTP status itself
    // is 200 because JSON-RPC carries errors in the body, not in
    // the HTTP status.
    assert!(status.is_success(), "JSON-RPC errors ride in body: {body}");
    assert_eq!(body["error"]["code"], -32001);
    assert_eq!(body["id"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resources_read_returns_runtime_stub() {
    // FR-A-6: `ui://triton/runtime.html` MAY be a stub. We return
    // a small text resource confirming the URI.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/read",
            "params": { "uri": "ui://triton/runtime.html" }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let contents = body["result"]["contents"]
        .as_array()
        .unwrap_or_else(|| panic!("contents array: {body}"));
    assert!(!contents.is_empty());
    assert_eq!(contents[0]["uri"], "ui://triton/runtime.html");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_method_returns_invalid_request_not_parse_error() {
    // Codex PR 7 finding: `{jsonrpc, id}` without `method` is valid
    // JSON, so it must be -32600 Invalid Request, NOT -32700 Parse Error.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({ "jsonrpc": "2.0", "id": 42 }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["error"]["code"], -32600);
    assert_eq!(body["id"], 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_returns_no_body() {
    // JSON-RPC 2.0: a request without `id` is a notification; the
    // server MUST NOT respond.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .post(mcp_url(&proc))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("POST notification");
    assert!(resp.status().is_success());
    let body = resp.bytes().await.expect("read body");
    assert!(
        body.is_empty(),
        "notification MUST NOT receive a body, got {body:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_rejects_unknown_protocol_version() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "initialize",
            "params": { "protocolVersion": "9999-99-99" }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["error"]["code"], -32602);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_method_emits_rejection_audit() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;
    let (_, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/nope",
            "params": {}
        }),
        Some("dev-token"),
    )
    .await;
    assert_eq!(body["error"]["code"], -32601);

    let rejected = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["tool"] == "tools/nope"
    });
    assert_eq!(rejected["protocol"], "mcp");
    assert_eq!(rejected["result"], "error:validation");
}

/// Issue #143 (A): when an upstream tool result carries MCP-Apps
/// `_meta.ui.*` fields (e.g. peacock's `render_report` returns
/// `_meta.ui.resourceUri = ui://peacock/r1`), Triton's MCP adapter MUST
/// surface them on the `tools/call` response `_meta`, alongside the
/// existing `trace_id`, without dropping unknown `ui.*` keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_call_passes_through_meta_ui_resource_uri() {
    let agent = FakeAgent::start_returning(json!({
        "report_id": "r1",
        "_meta": {
            "ui": {
                "resourceUri": "ui://peacock/r1",
                "preferredFrame": { "width": 720, "height": 480 }
            }
        }
    }))
    .await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_report={}", agent.host_port()),
            ),
        ]),
    )
    .await;

    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "render_report", "arguments": { "q": "suppliers" } }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}, body: {body}");

    let meta = &body["result"]["_meta"];
    // trace_id stays put.
    assert!(meta["trace_id"].is_string(), "trace_id missing: {body}");
    // The upstream's `_meta.ui.*` is lifted onto the response `_meta.ui`,
    // unknown sibling keys (`preferredFrame`) preserved verbatim.
    assert_eq!(
        meta["ui"]["resourceUri"], "ui://peacock/r1",
        "resourceUri not passed through: {body}"
    );
    assert_eq!(
        meta["ui"]["preferredFrame"]["width"], 720,
        "unknown ui.* key dropped: {body}"
    );
    // The structured envelope still carries the full result, including
    // its own `_meta` (back-compat for structured clients).
    assert_eq!(
        body["result"]["structuredContent"]["result"]["report_id"], "r1",
        "structured result changed: {body}"
    );
}

/// Issue #143 (B): `resources/read` of an upstream-owned `ui://` URI
/// proxies to the owning upstream and returns its bundle bytes; an
/// unknown owner still errors; the proxied call carries the minted
/// bearer + `X-Triton-MCP: resources/read` and emits one audit line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resources_read_proxies_to_owning_upstream() {
    let agent = FakeAgent::start_mcp_apps().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                // The tool key AND the `ui://` authority key both point at
                // peacock; resolution reuses the same registry (#143 B).
                format!("render_report={ep},peacock={ep}", ep = agent.host_port()),
            ),
        ]),
    )
    .await;

    // Owned URI → proxied bundle.
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "resources/read",
            "params": { "uri": "ui://peacock/r1" }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}, body: {body}");
    let contents = body["result"]["contents"]
        .as_array()
        .unwrap_or_else(|| panic!("contents array: {body}"));
    assert_eq!(contents[0]["uri"], "ui://peacock/r1", "{body}");
    assert_eq!(contents[0]["mimeType"], "text/html", "{body}");
    assert!(
        contents[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("peacock bundle"),
        "upstream bundle not returned: {body}"
    );

    // The upstream saw the MCP-Apps verb + a minted bearer.
    assert_eq!(
        agent.mcp_verbs_seen(),
        vec![Some("resources/read".to_string())],
        "upstream did not see the resources/read verb"
    );
    assert_eq!(agent.bearers_seen(), vec!["dev-token".to_string()]);

    // One audit line for the proxied read, keyed on the URI.
    let audit = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["tool"] == "ui://peacock/r1"
    });
    assert_eq!(audit["protocol"], "mcp");
    assert_eq!(audit["result"], "ok");

    // The stub stays served locally (no upstream hop).
    let (_, stub) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "resources/read",
            "params": { "uri": "ui://triton/runtime.html" }
        }),
        Some("dev-token"),
    )
    .await;
    assert_eq!(stub["result"]["contents"][0]["mimeType"], "text/html");

    // Unknown owner → JSON-RPC error, no panic.
    let (_, unknown) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "resources/read",
            "params": { "uri": "ui://ghost/x" }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(
        unknown["error"]["code"].is_number(),
        "unknown owner should error: {unknown}"
    );
}

/// Issue #143 (C): an in-iframe `callServerTool('render_report', {abs})`
/// reaches Triton as a normal `tools/call` and MUST dispatch to the
/// owning upstream as a fresh, stateless call — absolute params, never
/// deltas — each re-render returning the UI resource link. Two
/// re-renders with different absolute params hit the upstream twice with
/// exactly the bodies sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_server_tool_redispatches_with_absolute_params() {
    let agent = FakeAgent::start_mcp_apps().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_report={}", agent.host_port()),
            ),
        ]),
    )
    .await;

    for (id, region) in [(21, "emea"), (22, "apac")] {
        let (status, body) = rpc_request(
            &proc,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "render_report",
                    "arguments": { "region": region, "limit": 50 }
                }
            }),
            Some("dev-token"),
        )
        .await;
        assert!(status.is_success(), "status: {status}, body: {body}");
        // Each re-render hands back the (re-rendered) UI resource link.
        assert_eq!(
            body["result"]["_meta"]["ui"]["resourceUri"],
            "ui://peacock/r1"
        );
    }

    // Two fresh dispatches, each carrying its absolute params verbatim.
    let bodies = agent.bodies_seen();
    assert_eq!(
        bodies.len(),
        2,
        "expected two stateless dispatches: {bodies:?}"
    );
    assert_eq!(bodies[0]["region"], "emea");
    assert_eq!(bodies[0]["limit"], 50);
    assert_eq!(bodies[1]["region"], "apac");
    // Plain tool calls — no MCP-Apps verb header.
    assert_eq!(agent.mcp_verbs_seen(), vec![None, None]);
}

/// Issue #143 (C): a host's `updateModelContext` record pushed from the
/// iframe MUST be relayed to the owning upstream **unmodified** — Triton
/// neither inspects nor expands the compact `{report_id, params,
/// salient_summary}` payload. Routing is by the resource `uri`; the
/// record rides `X-Triton-MCP: updateModelContext` byte-for-byte, under
/// the same minted bearer + audit as any dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_model_context_relays_record_unmodified() {
    let agent = FakeAgent::start_mcp_apps().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("peacock={}", agent.host_port()),
            ),
        ]),
    )
    .await;

    let record = json!({
        "report_id": "r1",
        "params": { "region": "emea", "limit": 50 },
        "salient_summary": "3 high-risk suppliers; top: Acme (0.91)"
    });
    let (status, body) = rpc_request(
        &proc,
        json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "updateModelContext",
            "params": { "uri": "ui://peacock/r1", "record": record }
        }),
        Some("dev-token"),
    )
    .await;
    assert!(status.is_success(), "status: {status}, body: {body}");
    assert_eq!(body["result"]["relayed"], true, "not relayed: {body}");

    // The upstream saw the verb and the record verbatim — no wrapping.
    assert_eq!(
        agent.mcp_verbs_seen(),
        vec![Some("updateModelContext".to_string())]
    );
    let seen = agent.bodies_seen();
    assert_eq!(seen.len(), 1, "expected one relay: {seen:?}");
    assert_eq!(
        seen[0], record,
        "record was modified in flight: {:?}",
        seen[0]
    );
    assert_eq!(agent.bearers_seen(), vec!["dev-token".to_string()]);

    // One audit line for the relay, keyed on the resource URI.
    let audit = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["tool"] == "ui://peacock/r1"
    });
    assert_eq!(audit["protocol"], "mcp");
    assert_eq!(audit["result"], "ok");
}

fn wait_for_audit_matching<F>(
    proc: &TritonProcess,
    deadline: Duration,
    mut matches: F,
) -> serde_json::Value
where
    F: FnMut(&serde_json::Value) -> bool,
{
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if matches(&v) {
                return v;
            }
        }
        if start.elapsed() > deadline {
            panic!(
                "audit line not found within {deadline:?}\nstdout:\n{}",
                proc.stdout_snapshot().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
