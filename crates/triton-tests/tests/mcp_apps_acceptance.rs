//! Issue #143 — MCP-Apps proxying acceptance sweep.
//!
//! Per-capability behaviour is tested in `mcp.rs` (A/B/C) and
//! `telegram_dashboard_upstream.rs` (D). This file proves the increments
//! *compose* against a single MCP-Apps upstream in one real Triton, and
//! that a load-bearing egress control — the per-tool circuit breaker —
//! applies to the new proxied `resources/read` path just like a
//! `tools/call` (acceptance criterion 5).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

async fn rpc(proc: &TritonProcess, body: Value) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("http://{}/", proc.mcp_addr))
        .bearer_auth("dev-token")
        .json(&body)
        .send()
        .await
        .expect("POST mcp");
    resp.json().await.expect("decode mcp body")
}

/// A (pass-through) + B (resources/read proxy) + C (callServerTool
/// re-render + updateModelContext relay) all driven against one upstream
/// in one process, with the audit trail proving each proxied hop emitted
/// its single `phase: dispatch` line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_apps_capabilities_compose_against_one_upstream() {
    let agent = FakeAgent::start_mcp_apps().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("render_report={ep},peacock={ep}", ep = agent.host_port()),
            ),
        ]),
    )
    .await;

    // A — tools/call returns the UI resource link.
    let a = rpc(
        &proc,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "render_report", "arguments": { "region": "emea" } }
        }),
    )
    .await;
    assert_eq!(a["result"]["_meta"]["ui"]["resourceUri"], "ui://peacock/r1");

    // B — resources/read of that URI proxies to peacock.
    let b = rpc(
        &proc,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "resources/read",
            "params": { "uri": "ui://peacock/r1" }
        }),
    )
    .await;
    assert!(
        b["result"]["contents"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("peacock bundle"),
        "B: bundle not proxied: {b}"
    );

    // C — callServerTool re-render (a fresh tools/call) dispatches again.
    let c1 = rpc(
        &proc,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "render_report", "arguments": { "region": "apac" } }
        }),
    )
    .await;
    assert_eq!(
        c1["result"]["_meta"]["ui"]["resourceUri"],
        "ui://peacock/r1"
    );

    // C — updateModelContext is relayed unmodified.
    let record =
        json!({ "report_id": "r1", "params": { "region": "apac" }, "salient_summary": "ok" });
    let c2 = rpc(
        &proc,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "updateModelContext",
            "params": { "uri": "ui://peacock/r1", "record": record }
        }),
    )
    .await;
    assert_eq!(
        c2["result"]["relayed"], true,
        "C: updateModelContext not relayed: {c2}"
    );

    // The upstream saw: 2 tool calls (no verb), 1 resources/read, 1
    // updateModelContext — proving every path reached it.
    let verbs = agent.mcp_verbs_seen();
    assert_eq!(
        verbs.iter().filter(|v| v.is_none()).count(),
        2,
        "verbs: {verbs:?}"
    );
    assert!(verbs.contains(&Some("resources/read".to_string())));
    assert!(verbs.contains(&Some("updateModelContext".to_string())));

    // One audit `phase: dispatch` line per proxied hop. The two proxied
    // ui:// ops audit under the URI; the tool calls under the tool name.
    let audits = wait_for_audits(&proc, Duration::from_secs(2), 4);
    let dispatch = |tool: &str| {
        audits
            .iter()
            .filter(|v| v["phase"] == "dispatch" && v["tool"] == tool && v["result"] == "ok")
            .count()
    };
    assert_eq!(dispatch("render_report"), 2, "two tool dispatches");
    assert_eq!(dispatch("ui://peacock/r1"), 2, "read + updateModelContext");
}

/// Acceptance criterion 5: the per-tool circuit breaker that guards a
/// `tools/call` applies identically to the proxied `resources/read` path.
/// A sick renderer that 500s every read trips the breaker after
/// `TRITON_CIRCUIT_OPEN_AFTER` faults; further reads fail fast without
/// dialling the upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resources_read_breaker_trips_on_sick_renderer() {
    let agent = FakeAgent::start_always_failing().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([
            ("TRITON_ENV".to_string(), "nonprod".to_string()),
            (
                "TRITON_STATIC_UPSTREAMS".to_string(),
                format!("peacock={}", agent.host_port()),
            ),
            ("TRITON_CIRCUIT_OPEN_AFTER".to_string(), "2".to_string()),
            (
                "TRITON_CIRCUIT_COOLDOWN_MS".to_string(),
                "60000".to_string(),
            ),
        ]),
    )
    .await;

    let read = || async {
        rpc(
            &proc,
            json!({
                "jsonrpc": "2.0", "id": 9, "method": "resources/read",
                "params": { "uri": "ui://peacock/r1" }
            }),
        )
        .await
    };

    // First two reads reach the sick renderer and error (agent-side fault).
    assert!(read().await["error"]["code"].is_number(), "1st read errors");
    assert!(
        read().await["error"]["code"].is_number(),
        "2nd read trips breaker"
    );
    // Breaker now open: the 3rd read fails fast and never dials the agent.
    let third = read().await;
    assert!(third["error"]["code"].is_number(), "3rd read fails fast");
    assert!(
        third["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("circuit_open"),
        "breaker-open error expected, got: {third}"
    );
    assert_eq!(
        agent.hits(),
        2,
        "open breaker must short-circuit before dialling the renderer"
    );
}

fn wait_for_audits(proc: &TritonProcess, deadline: Duration, at_least: usize) -> Vec<Value> {
    let start = Instant::now();
    loop {
        let audits: Vec<Value> = proc
            .stdout_snapshot()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["kind"] == "audit" && v["phase"] == "dispatch")
            .collect();
        if audits.len() >= at_least {
            return audits;
        }
        if start.elapsed() > deadline {
            return audits;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
