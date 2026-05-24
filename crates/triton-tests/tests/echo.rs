//! Dispatcher + audit emitter end-to-end. PR 4 wires the smallest
//! tool (`echo`) through the central `ToolRegistry::invoke` path and
//! asserts the audit line emitted on stdout carries the FR-AU-2
//! field set with the same `trace_id` the response reports.
//!
//! No mocks: real binary, real HTTP, real stdout.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_tool_round_trips_and_emits_audit_line() {
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hello dispatcher" }))
        .send()
        .await
        .expect("POST /v1/tools/echo");
    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode JSON");
    assert_eq!(
        body["result"]["echo"], "hello dispatcher",
        "unexpected response body: {body}"
    );
    let trace_id = body["trace_id"]
        .as_str()
        .expect("trace_id present in response")
        .to_string();
    assert!(!trace_id.is_empty(), "trace_id non-empty: {body}");

    // Audit lines are emitted by a synchronous println! inside the
    // dispatcher; they should reach stdout before the HTTP response
    // returns. Poll briefly to absorb pipe-collector scheduling.
    let audit = wait_for_audit_with_trace_id(&proc, &trace_id, Duration::from_secs(2));

    assert_eq!(audit["kind"], "audit");
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["what"], "echo");
    assert_eq!(audit["who"], "dev-user");
    assert_eq!(audit["subject"], "dev-user");
    assert_eq!(audit["tenant"], "dev");
    assert_eq!(audit["env"], "nonprod");
    assert_eq!(audit["protocol"], "rest");
    assert_eq!(audit["result"], "ok");
    assert_eq!(audit["status"], 200);
    assert!(
        audit["latency_ms"].as_u64().is_some(),
        "latency_ms is a number: {audit}"
    );
    // FR-AU-3: tokens MUST NEVER appear in audit lines.
    let serialised = audit.to_string();
    assert!(
        !serialised.contains("dev-token"),
        "audit line leaked the raw token: {serialised}"
    );
    // RFC 3339 UTC timestamp.
    let when = audit["when"].as_str().expect("when is a string");
    assert!(when.ends_with('Z'), "when should be UTC: {when}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_bearer_emits_rejected_audit_line() {
    // ADR-15 / FR-AU-1: even boundary rejections (auth fails before
    // the dispatcher would run) MUST surface an audit line so a
    // single query keyed on `trace_id` returns the complete causal
    // chain. Adapters delegate the emission to the dispatcher
    // (ADR-6) so the schema lives in one place.
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .json(&json!({ "message": "hello" }))
        .send()
        .await
        .expect("POST /v1/tools/echo");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_any_rejected_audit(&proc, Duration::from_secs(2));
    assert_eq!(rejected["kind"], "audit");
    assert_eq!(rejected["phase"], "rejected");
    assert_eq!(rejected["tool"], "echo");
    assert_eq!(rejected["result"], "error:auth");
    assert_eq!(rejected["status"], 401);
    assert_eq!(rejected["protocol"], "rest");
    assert_eq!(rejected["env"], "nonprod");
}

fn wait_for_audit_with_trace_id(
    proc: &TritonProcess,
    trace_id: &str,
    deadline: Duration,
) -> serde_json::Value {
    wait_for_audit(proc, deadline, |v| {
        v["kind"] == "audit" && v["trace_id"] == trace_id
    })
}

fn wait_for_any_rejected_audit(proc: &TritonProcess, deadline: Duration) -> serde_json::Value {
    wait_for_audit(proc, deadline, |v| {
        v["kind"] == "audit" && v["phase"] == "rejected"
    })
}

fn wait_for_audit<F>(proc: &TritonProcess, deadline: Duration, mut matches: F) -> serde_json::Value
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
                "audit line not found within {deadline:?}\n\
                 stdout snapshot ({} lines):\n{}\n\
                 stderr snapshot ({} lines):\n{}",
                proc.stdout_snapshot().len(),
                proc.stdout_snapshot().join("\n"),
                proc.stderr_snapshot().len(),
                proc.stderr_snapshot().join("\n"),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
