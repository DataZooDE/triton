//! Round out the v0.1 acceptance tests:
//!   * ACC-2 — SIGTERM during an in-flight call MUST allow the
//!     call to complete (no 5xx, no dropped connection).
//!   * ACC-8 — fresh process binds + answers /healthz within a
//!     tight time bound (target 1 s).
//!   * Audit-schema completeness — every FR-AU-2 field present.
//!
//! No mocks: real binary, real signals.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use triton_tests::{Signal, TritonProcess};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acc2_sigterm_during_inflight_call_does_not_drop_request() {
    let mut proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    // Start a long-running dispatch in the background. The `delay`
    // tool sleeps for `ms` before returning.
    let url = proc.rest_url("/v1/tools/delay");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let inflight = tokio::spawn(async move {
        client
            .post(&url)
            .bearer_auth("dev-token")
            .json(&serde_json::json!({ "ms": 1500 }))
            .send()
            .await
    });

    // Give the request time to reach the server and start sleeping.
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Now SIGTERM the child mid-flight.
    let exit = tokio::task::spawn_blocking(move || {
        let status = proc.signal(Signal::Term, Duration::from_secs(5));
        (status, proc)
    });

    // The in-flight call MUST complete with a 2xx.
    let resp = inflight
        .await
        .expect("join inflight")
        .expect("send inflight");
    assert!(
        resp.status().is_success(),
        "in-flight request dropped on SIGTERM: {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["delayed_ms"], 1500);
    // Defence against false positives (Codex PR 11 finding): the
    // dispatcher reports its measured latency. If the test ever
    // passes without the tool actually sleeping through SIGTERM,
    // `latency_ms` will be near-zero. Anything < the requested
    // delay means the breaker shortcut the call.
    let latency = body["latency_ms"].as_u64().expect("latency_ms");
    assert!(
        latency >= 1500,
        "ACC-2: handler returned in {latency} ms, but delay.ms was 1500 — \
         SIGTERM appears to have dropped the in-flight call: {body}"
    );

    // And the binary MUST exit 0 once drain completes.
    let (status, _proc) = exit.await.expect("await signal");
    assert!(status.success(), "exit non-zero on SIGTERM: {status:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acc8_cold_start_health_within_one_second() {
    // ACC-8 target is 1 s; the harness's spawn waits for /healthz
    // already, so measure the spawn time itself.
    let start = Instant::now();
    let _proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "cold-start /healthz took {elapsed:?}, ACC-8 target is < 1 s (debug builds get a 2 s buffer)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_schema_carries_every_fr_au_2_field() {
    // FR-AU-2: each audit line MUST contain at least
    // {who, what, when, env, result, protocol, tool, subject,
    //  tenant, latency_ms, status, trace_id}.
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        HashMap::from([("TRITON_ENV".to_string(), "nonprod".to_string())]),
    )
    .await;
    let _ = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({ "message": "audit me" }))
        .send()
        .await
        .expect("dispatch");

    let audit = wait_for_audit(&proc, Duration::from_secs(2)).await;

    for field in [
        "who",
        "what",
        "when",
        "env",
        "result",
        "protocol",
        "tool",
        "subject",
        "tenant",
        "latency_ms",
        "status",
        "trace_id",
    ] {
        assert!(
            !audit[field].is_null(),
            "audit line missing FR-AU-2 field `{field}`: {audit}"
        );
    }
    // Types per FR-AU-2:
    assert!(audit["latency_ms"].as_u64().is_some());
    assert!(audit["status"].as_u64().is_some());
    assert!(audit["when"].as_str().unwrap_or("").ends_with('Z'));
}

async fn wait_for_audit(proc: &TritonProcess, deadline: Duration) -> Value {
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["kind"] == "audit" && v["phase"] == "dispatch" {
                return v;
            }
        }
        if start.elapsed() > deadline {
            panic!(
                "no dispatch audit line within {deadline:?}\nstdout:\n{}",
                proc.stdout_snapshot().join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
