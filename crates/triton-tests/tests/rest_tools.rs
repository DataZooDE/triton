//! REST `/v1/tools` listing (FR-A-5) and `Accept: application/json+a2ui`
//! content negotiation (FR-A-3). PR 10 adds the actual A2UI
//! wrapping; PR 5 only confirms the negotiation parses correctly
//! and the listing exposes the documented schema.
//!
//! No mocks: real binary, real HTTP.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_v1_tools_lists_echo_with_schema() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("decode JSON");

    let tools = body["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("expected `tools` to be an array: {body}"));
    let echo = tools
        .iter()
        .find(|t| t["name"] == "echo")
        .unwrap_or_else(|| panic!("echo not found in tools: {body}"));

    assert_eq!(
        echo["returns_a2ui"], false,
        "echo does not return A2UI surface: {echo}"
    );
    // FR-A-5: each entry MUST carry the input JSON schema. For echo
    // that's a single required string field `message`.
    let schema = &echo["input_schema"];
    assert_eq!(
        schema["type"], "object",
        "schema should be object: {schema}"
    );
    assert_eq!(
        schema["required"].as_array().expect("required is array"),
        &vec![json!("message")],
        "echo schema must require `message`: {schema}"
    );
    assert_eq!(
        schema["properties"]["message"]["type"], "string",
        "message must be string: {schema}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_v1_tools_requires_auth_and_audits_rejection() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .send()
        .await
        .expect("GET /v1/tools");
    assert_eq!(resp.status(), 401, "unauth listing must 401");

    // FR-AU-1 / ADR-15: a rejected boundary call MUST still produce
    // an audit line — the listing surface is no exception.
    let rejected = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["tool"] == "v1/tools"
    });
    assert_eq!(rejected["protocol"], "rest");
    assert_eq!(rejected["result"], "error:auth");
    assert_eq!(rejected["status"], 401);
    assert_eq!(rejected["env"], "nonprod");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_tool_rejects_unknown_a2ui_version() {
    // FR-A-3: only `application/json+a2ui` with no version or
    // `version=0.9` are honoured. Any other version is an explicit
    // error so the caller learns about the drift.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=99.0")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/echo");
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "validation");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_tool_accepts_a2ui_v09_negotiation() {
    // FR-A-3: REST MUST honour `Accept: application/json+a2ui;
    // version=0.9`. PR 5 only confirms the request is *accepted*
    // (no 415 / Not Acceptable); the actual A2UI wrapping lands in
    // PR 10. The echo tool does not return a surface, so the
    // response payload stays plain JSON regardless of the Accept
    // header — but a misparsed header would 4xx, which is what we
    // guard against.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=0.9")
        .json(&json!({ "message": "hi a2ui" }))
        .send()
        .await
        .expect("POST /v1/tools/echo");
    assert!(
        resp.status().is_success(),
        "v0.9 Accept must not be rejected, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["echo"], "hi a2ui");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_tool_malformed_body_returns_400() {
    // architecture.md §8.3: TritonError::Validation maps to 400.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .header("Content-Type", "application/json")
        .body("{not-json")
        .send()
        .await
        .expect("POST malformed body");
    assert_eq!(resp.status(), 400, "malformed JSON body must 400");
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "validation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_unknown_tool_returns_400() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/no-such-tool"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST unknown tool");
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "validation");
}
