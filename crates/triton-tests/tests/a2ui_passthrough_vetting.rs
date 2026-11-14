//! #370 — producer-built A2UI passthrough surfaces (`surface.a2ui`) are
//! validated against the A2UI v0.9 schema of the catalog they declare
//! before any face renders them.
//!
//! The user story: a Gemini Enterprise user asks the agent; the agent
//! answers with prose, one good native card and two broken ones. The user
//! sees the good card (and Triton's own card); the broken ones are dropped
//! instead of blanking the reply, and the operator sees one `warn` line per
//! dropped surface (schema path + component id, never a value) plus a
//! `triton_a2ui_passthrough_dropped_total` counter.
//!
//! No mocks: spawned `triton` binary, real HTTP, in-repo fake upstream.

use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

use crate::a2a_bin::a2a_env;

const BASIC: &str = "https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json";
const SECRET: &str = "s3cr3t-value";

/// (a) valid GE-composite surface, no `catalogId` (back-compat default).
fn good_ge_surface() -> Value {
    json!({
        "components": [
            { "id": "root", "component": "MaterialCard", "children": ["t"] },
            { "id": "t", "component": "MaterialText", "text": "Coverage looks fine." }
        ],
        "theme": { "agentDisplayName": "Coverage Matching" }
    })
}

/// The agent's answer: prose, one good surface, then the given bad ones.
fn answer(bad: Vec<Value>) -> Value {
    let mut surfaces = vec![good_ge_surface()];
    surfaces.extend(bad);
    json!({ "surface": {
        "components": [
            { "kind": "text", "value": "Three close calls." },
            { "kind": "key_value", "pairs": [ { "label": "Open", "value": "3" } ] }
        ],
        "a2ui": surfaces
    }})
}

/// (b) unknown component, carrying a value that must never reach a log.
fn unknown_component() -> Value {
    json!({ "catalogId": BASIC, "components": [
        { "id": "root", "component": "NoSuchWidget", "note": SECRET }
    ]})
}

/// (c) a basic Button without its required `action`.
fn button_without_action() -> Value {
    json!({ "catalogId": BASIC, "components": [
        { "id": "root", "component": "Column", "children": ["b"] },
        { "id": "b", "component": "Button", "child": "l" },
        { "id": "l", "component": "Text", "text": SECRET }
    ]})
}

async fn spawn(body: Value) -> (TritonProcess, FakeAgent) {
    let agent = FakeAgent::start_returning(body).await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(&agent, true)).await;
    (triton, agent)
}

fn send_message(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": { "message": {
        "kind": "message", "role": "user", "messageId": "m-1",
        "parts": [ { "kind": "text", "text": "Which claims are close calls?" } ]
    }}})
}

async fn post(triton: &TritonProcess, method: &str) -> reqwest::Response {
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/a2a"))
        .bearer_auth("dev-token")
        .header("X-A2A-Extensions", triton_core::a2ui::ge::EXTENSION_URI)
        .json(&send_message(method))
        .send()
        .await
        .expect("POST /a2a");
    assert!(resp.status().is_success(), "{}", resp.status());
    resp
}

/// Every A2UI message in a JSON value (the data parts, wherever they sit).
fn a2ui_messages(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            if m.get("kind").and_then(Value::as_str) == Some("data")
                && let Some(d) = m.get("data")
                && d.get("version").and_then(Value::as_str) == Some("v0.9")
            {
                out.push(d.clone());
            }
            m.values().for_each(|x| a2ui_messages(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| a2ui_messages(x, out)),
        _ => {}
    }
}

fn passthrough_ids(msgs: &[Value]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| m["createSurface"]["surfaceId"].as_str())
        .filter(|id| id.starts_with("triton-a2ui-"))
        .map(String::from)
        .collect()
}

/// The JSON log lines whose message names a dropped passthrough surface.
async fn drop_warnings(triton: &TritonProcess, want: usize) -> Vec<Value> {
    for _ in 0..50 {
        let lines: Vec<Value> = triton
            .stdout_snapshot()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| {
                v["fields"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("a2ui passthrough surface dropped"))
            })
            .collect();
        if lines.len() >= want {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "expected {want} drop warnings in:\n{:#?}",
        triton.stdout_snapshot()
    );
}

async fn metrics(triton: &TritonProcess) -> String {
    reqwest::Client::new()
        .get(triton.rest_url("/v1/metrics"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/metrics")
        .text()
        .await
        .expect("metrics text")
}

fn assert_no_secret_logged(triton: &TritonProcess) {
    for line in triton
        .stdout_snapshot()
        .iter()
        .chain(triton.stderr_snapshot().iter())
    {
        assert!(
            !line.contains(SECRET),
            "a component value was logged: {line}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_invalid_passthrough_surfaces_are_dropped_and_reported() {
    let (triton, _agent) = spawn(answer(vec![unknown_component(), button_without_action()])).await;

    let reply: Value = post(&triton, "message/send")
        .await
        .json()
        .await
        .expect("json");
    let mut msgs = Vec::new();
    a2ui_messages(&reply["result"], &mut msgs);

    let ids = passthrough_ids(&msgs);
    assert_eq!(ids.len(), 1, "only the valid surface survives: {msgs:#?}");
    let comps = msgs
        .iter()
        .find(|m| m["updateComponents"]["surfaceId"] == ids[0].as_str())
        .expect("updateComponents of the valid surface");
    assert_eq!(
        comps["updateComponents"]["components"],
        good_ge_surface()["components"],
        "forwarded verbatim"
    );
    assert!(
        msgs.iter().any(|m| m["createSurface"]["surfaceId"]
            .as_str()
            .is_some_and(|id| !id.starts_with("triton-a2ui-"))),
        "Triton's own card still renders: {msgs:#?}"
    );

    let warns = drop_warnings(&triton, 2).await;
    assert_eq!(warns.len(), 2, "{warns:#?}");
    for w in &warns {
        assert_eq!(w["level"], "WARN", "{w}");
        let f = &w["fields"];
        assert_eq!(f["tool"], "ask_portfolio_agent", "{w}");
        assert_eq!(f["reason"], "schema", "{w}");
        assert!(
            f["schema_path"].as_str().is_some_and(|p| !p.is_empty()),
            "{w}"
        );
        assert!(f["component_id"].is_string(), "{w}");
    }
    let indices: Vec<u64> = warns
        .iter()
        .filter_map(|w| w["fields"]["surface_index"].as_u64())
        .collect();
    assert_eq!(indices, vec![1, 2], "{warns:#?}");
    assert_eq!(warns[0]["fields"]["component_id"], "root");
    assert_eq!(warns[1]["fields"]["component_id"], "b");
    assert_no_secret_logged(&triton);

    let m = metrics(&triton).await;
    assert!(
        m.contains(r#"triton_a2ui_passthrough_dropped_total{reason="schema"} 2"#),
        "{m}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_catalog_is_dropped() {
    let foreign = json!({
        "catalogId": "https://example.test/my-catalog.json",
        "components": [ { "id": "root", "component": "Text", "text": "hi" } ]
    });
    let (triton, _agent) = spawn(answer(vec![foreign])).await;

    let reply: Value = post(&triton, "message/send")
        .await
        .json()
        .await
        .expect("json");
    let mut msgs = Vec::new();
    a2ui_messages(&reply["result"], &mut msgs);
    assert_eq!(passthrough_ids(&msgs).len(), 1, "{msgs:#?}");

    let warns = drop_warnings(&triton, 1).await;
    assert_eq!(warns[0]["fields"]["reason"], "catalog", "{warns:#?}");
    let m = metrics(&triton).await;
    assert!(
        m.contains(r#"triton_a2ui_passthrough_dropped_total{reason="catalog"} 1"#),
        "{m}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_answer_is_vetted_too() {
    let (triton, _agent) = spawn(answer(vec![unknown_component()])).await;

    let body = post(&triton, "message/stream")
        .await
        .text()
        .await
        .expect("sse");
    let mut msgs = Vec::new();
    for frame in body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
    {
        a2ui_messages(&frame, &mut msgs);
    }
    assert_eq!(passthrough_ids(&msgs).len(), 1, "{body}");
    let warns = drop_warnings(&triton, 1).await;
    assert_eq!(warns[0]["fields"]["reason"], "schema");
    assert_no_secret_logged(&triton);
}
