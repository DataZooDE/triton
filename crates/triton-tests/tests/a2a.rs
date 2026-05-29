//! A2A adapter — `POST /message:send` per FR-A-7. Drives the echo
//! tool through the same dispatcher as REST and asserts the inner
//! result is byte-equal (parsed dicts) across the two protocols.
//! This is the groundwork for ACC-1 (full parity once MCP lands in
//! PR 7).
//!
//! No mocks: real binary, real HTTP, real audit lines.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_routes_echo_through_dispatcher() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [
                { "data": { "tool": "echo", "args": { "message": "hi via a2a" } } }
            ]
        }))
        .send()
        .await
        .expect("POST /message:send");
    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(
        body["parts"][0]["data"]["result"]["echo"], "hi via a2a",
        "unexpected response: {body}"
    );
    let trace_id = body["parts"][0]["data"]["trace_id"]
        .as_str()
        .expect("trace_id present")
        .to_string();
    assert!(!trace_id.is_empty());

    // FR-A-7: the in-process task store records this trace as
    // Completed and echoes the terminal state in the response
    // metadata. The explorer's Adapters page surfaces this as the
    // "task: completed" chip — A2A is the only protocol carrying a
    // task lifecycle, so pin the wire shape it depends on.
    assert_eq!(
        body["metadata"]["task_state"], "completed",
        "expected metadata.task_state=completed: {body}"
    );

    // FR-AU-1 audit line must arrive with protocol=a2a.
    let audit = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["trace_id"] == trace_id
    });
    assert_eq!(audit["protocol"], "a2a");
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_failure_omits_task_state_from_error_envelope() {
    // The task store records a failed invocation as Failed internally,
    // but the A2A *error* envelope carries `metadata.{error,message}`
    // (+ trace_id) only — never a task_state. The explorer relies on
    // this: the "task" chip shows on success and stays absent on
    // error, where the error card carries the detail instead.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "__missing__", "args": {} } }]
        }))
        .send()
        .await
        .expect("POST unknown tool");
    assert!(!resp.status().is_success(), "expected non-2xx");
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert!(
        body["metadata"]["task_state"].is_null(),
        "error envelope must not carry task_state: {body}"
    );
    assert!(
        body["metadata"]["error"].is_string(),
        "error envelope carries an error class: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_and_a2a_responses_match_dict_parity() {
    // Groundwork for ACC-1: same input + principal must produce the
    // same inner `result` across protocols. The outer envelopes
    // differ (REST `{result, trace_id, latency_ms}` vs A2A
    // `Message{parts:[Part{data:{...}}]}`), but the inner result
    // dict is what callers care about.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let client = reqwest::Client::new();
    let args = json!({ "message": "parity check" });

    let rest_body: serde_json::Value = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&args)
        .send()
        .await
        .expect("REST POST")
        .json()
        .await
        .expect("decode REST");
    let a2a_body: serde_json::Value = client
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "echo", "args": args } }]
        }))
        .send()
        .await
        .expect("A2A POST")
        .json()
        .await
        .expect("decode A2A");

    assert_eq!(
        rest_body["result"], a2a_body["parts"][0]["data"]["result"],
        "REST and A2A inner result diverged: REST={rest_body} A2A={a2a_body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_without_auth_emits_rejection_audit() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .json(&json!({ "parts": [{ "data": { "tool": "echo", "args": {} } }] }))
        .send()
        .await
        .expect("POST /message:send");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "a2a"
    });
    assert_eq!(rejected["result"], "error:auth");
    assert_eq!(rejected["status"], 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_malformed_body_emits_validation_audit() {
    // Codex blocker fix: a malformed JSON body must flow through
    // record_rejection (FR-AU-1), not axum's silent 422.
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .header("Content-Type", "application/json")
        .body("{not-json")
        .send()
        .await
        .expect("POST malformed body");
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "validation");

    let rejected = wait_for_audit_matching(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "a2a"
    });
    assert_eq!(rejected["result"], "error:validation");
    assert_eq!(rejected["status"], 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_rejects_unknown_a2ui_version() {
    // FR-A-3: A2A MUST honour `metadata.a2ui_version`; unknown
    // values are an explicit error, not a silent downgrade.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "echo", "args": { "message": "x" } } }],
            "metadata": { "a2ui_version": "v99" }
        }))
        .send()
        .await
        .expect("POST unknown version");
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["error"], "validation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_with_missing_part_returns_validation() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/message:send", proc.a2a_addr))
        .bearer_auth("dev-token")
        .json(&json!({ "parts": [] }))
        .send()
        .await
        .expect("POST empty parts");
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
