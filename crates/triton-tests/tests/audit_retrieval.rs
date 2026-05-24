//! `GET /v1/audit` — newest-first slice of the in-process audit
//! ring buffer. Lets the explorer's Audit page render the same
//! records the substrate log shipper would see, without the
//! operator scraping stdout.
//!
//! Acceptance:
//!   * Each successful invocation produces a `phase=dispatch` entry.
//!   * Each auth failure produces a `phase=rejected` entry.
//!   * The endpoint requires the same Bearer Triton accepts on
//!     /v1/tools — no anonymous reads.
//!   * `?trace_id=...` filters; `?limit=N` is capped server-side.
//!
//! No mocks: real binary, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_endpoint_returns_dispatch_and_rejected_records() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let client = reqwest::Client::new();

    // Fire one successful echo so the buffer gets a phase=dispatch.
    let echo_resp = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({ "message": "audit-test" }))
        .send()
        .await
        .expect("POST echo");
    assert!(echo_resp.status().is_success(), "echo should 200");
    let echo_body: serde_json::Value = echo_resp.json().await.expect("decode echo");
    let echo_trace = echo_body["trace_id"]
        .as_str()
        .expect("trace_id present")
        .to_string();

    // Fire one bogus-token call so the buffer gets a phase=rejected.
    let _ = client
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("not-a-real-token")
        .json(&serde_json::json!({ "message": "x" }))
        .send()
        .await;

    // Read back.
    let resp: serde_json::Value = client
        .get(proc.rest_url("/v1/audit"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET audit")
        .json()
        .await
        .expect("decode audit");

    let entries = resp["entries"].as_array().expect("entries array");
    assert!(
        entries.len() >= 2,
        "expected at least dispatch + rejected entries, got {entries:?}"
    );
    let phases: Vec<&str> = entries.iter().filter_map(|e| e["phase"].as_str()).collect();
    assert!(phases.contains(&"dispatch"), "no dispatch in {phases:?}");
    assert!(phases.contains(&"rejected"), "no rejected in {phases:?}");

    // Newest-first ordering: the first entry should have a later or
    // equal `when` than the last.
    let first_when = entries.first().unwrap()["when"].as_str().expect("when");
    let last_when = entries.last().unwrap()["when"].as_str().expect("when");
    assert!(
        first_when >= last_when,
        "expected newest-first ordering ({first_when} vs {last_when})"
    );

    // Filter by trace_id — the echo trace should appear at least once
    // and the result must NOT include unrelated entries.
    let filtered: serde_json::Value = client
        .get(proc.rest_url(&format!("/v1/audit?trace_id={echo_trace}")))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET filtered")
        .json()
        .await
        .expect("decode filtered");
    let filtered_entries = filtered["entries"].as_array().expect("entries");
    assert!(!filtered_entries.is_empty(), "no entries for trace_id");
    for e in filtered_entries {
        assert_eq!(
            e["trace_id"], echo_trace,
            "filter leaked unrelated entry: {e}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_endpoint_requires_auth() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/audit"))
        .send()
        .await
        .expect("GET audit");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "audit endpoint must require a Bearer token"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_endpoint_caps_limit_server_side() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/audit?limit=10000"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET audit")
        .json()
        .await
        .expect("decode");
    let cap = resp["limit"].as_u64().expect("limit field");
    assert!(cap <= 500, "server-side cap should clamp limit, got {cap}");
}
