//! Upstream router — Consul discovery + Vault swap + circuit
//! breaker (FR-U-1..5, FR-AU-1 outbound line). Real HTTP fakes for
//! Consul, Vault, and the upstream agent.
//!
//! No mocks: each fixture starts a tiny axum server speaking the
//! actual wire shape; the harness drives Triton against them
//! through real TCP.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::{FakeAgent, FakeConsul, FakeVault};

fn env_with(consul: &FakeConsul, vault: &FakeVault) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_CONSUL_URL".to_string(), consul.url()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        (
            "TRITON_VAULT_TOKEN".to_string(),
            "triton-vault-token".to_string(),
        ),
        (
            "TRITON_VAULT_OIDC_ROLE".to_string(),
            "agent-oidc-swap".to_string(),
        ),
        // Tight breaker for fast tests.
        ("TRITON_CIRCUIT_OPEN_AFTER".to_string(), "2".to_string()),
        ("TRITON_CIRCUIT_COOLDOWN_MS".to_string(), "200".to_string()),
        ("TRITON_UPSTREAM_TIMEOUT_MS".to_string(), "500".to_string()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_dispatch_happy_path_with_linked_audit() {
    // FR-U-1, FR-U-2, FR-U-5, FR-AU-1 outbound line.
    let agent = FakeAgent::start_echoing().await;
    let consul = FakeConsul::start(&[("stats", agent.host_port())]).await;
    let vault = FakeVault::start_minting("vault-minted-agent-token").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&consul, &vault)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/stats"))
        .bearer_auth("dev-token")
        .json(&json!({ "city": "Berlin" }))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    // FakeAgent echoes the args; Triton wraps in `result`.
    assert_eq!(body["result"]["echoed"]["city"], "Berlin");
    let trace_id = body["trace_id"]
        .as_str()
        .expect("trace_id present")
        .to_string();

    // FR-AU-1: TWO audit lines for one inbound call, both sharing
    // trace_id. One `phase: dispatch` from the dispatcher, one
    // `phase: upstream` from the router.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["trace_id"] == trace_id && v["phase"] == "dispatch"
    });
    let upstream = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["trace_id"] == trace_id && v["phase"] == "upstream"
    });
    assert_eq!(dispatch["protocol"], "rest");
    assert_eq!(upstream["protocol"], "upstream");
    assert_eq!(upstream["tool"], "stats");
    assert_eq!(upstream["result"], "ok");

    // FR-U-2 / NFR-S-3: the upstream MUST NOT see the inbound
    // raw token; it sees the Vault-minted one.
    let seen = agent.bearers_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], "vault-minted-agent-token");
    assert!(!seen[0].contains("dev-token"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_upstream_tool_returns_provider_error() {
    let consul = FakeConsul::start(&[]).await;
    let vault = FakeVault::start_minting("t").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&consul, &vault)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/no-such-agent"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    // Consul returns empty list → no endpoint → provider error.
    assert_eq!(resp.status(), 502);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn circuit_breaker_opens_after_consecutive_failures() {
    // FR-U-3 / FR-U-4. The fake agent is configured to 500 every
    // request; after CIRCUIT_OPEN_AFTER=2 failures, the breaker
    // opens and subsequent calls fail fast with circuit_open.
    let agent = FakeAgent::start_always_failing().await;
    let consul = FakeConsul::start(&[("flaky", agent.host_port())]).await;
    let vault = FakeVault::start_minting("t").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&consul, &vault)).await;
    let client = reqwest::Client::new();

    // Two failures fill the breaker.
    for _ in 0..2 {
        let r = client
            .post(proc.rest_url("/v1/tools/flaky"))
            .bearer_auth("dev-token")
            .json(&json!({}))
            .send()
            .await
            .expect("POST");
        assert_eq!(r.status(), 502);
    }
    // Reset the upstream's hit counter so we can prove no further
    // requests reach it once the breaker is open.
    agent.reset_hits();
    let r = client
        .post(proc.rest_url("/v1/tools/flaky"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.expect("decode");
    assert_eq!(body["error"], "tool");
    assert!(
        body["message"].as_str().unwrap().contains("circuit_open"),
        "expected circuit_open: {body}"
    );
    assert_eq!(
        agent.hits(),
        0,
        "no upstream call should have reached the agent while breaker was open"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn circuit_breaker_is_isolated_per_tool() {
    let flaky_agent = FakeAgent::start_always_failing().await;
    let healthy_agent = FakeAgent::start_echoing().await;
    let consul = FakeConsul::start(&[
        ("flaky", flaky_agent.host_port()),
        ("healthy", healthy_agent.host_port()),
    ])
    .await;
    let vault = FakeVault::start_minting("t").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&consul, &vault)).await;
    let client = reqwest::Client::new();

    // Trip `flaky`'s breaker.
    for _ in 0..2 {
        let _ = client
            .post(proc.rest_url("/v1/tools/flaky"))
            .bearer_auth("dev-token")
            .json(&json!({}))
            .send()
            .await
            .expect("POST");
    }
    let flaky = client
        .post(proc.rest_url("/v1/tools/flaky"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    assert_eq!(flaky.status(), 503);

    // `healthy` is unaffected.
    let healthy = client
        .post(proc.rest_url("/v1/tools/healthy"))
        .bearer_auth("dev-token")
        .json(&json!({ "ok": true }))
        .send()
        .await
        .expect("POST");
    assert!(
        healthy.status().is_success(),
        "healthy tool was wedged by flaky's breaker: {}",
        healthy.status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn circuit_breaker_recovers_after_cooldown() {
    // After cooldown, the breaker enters half-open and lets a
    // probe through. A successful probe closes the breaker.
    let agent = FakeAgent::start_failing_then_recovering(2).await;
    let consul = FakeConsul::start(&[("recover", agent.host_port())]).await;
    let vault = FakeVault::start_minting("t").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&consul, &vault)).await;
    let client = reqwest::Client::new();

    // First two fail → breaker opens.
    for _ in 0..2 {
        let _ = client
            .post(proc.rest_url("/v1/tools/recover"))
            .bearer_auth("dev-token")
            .json(&json!({}))
            .send()
            .await
            .expect("POST");
    }
    // Third → 503 circuit_open.
    let r = client
        .post(proc.rest_url("/v1/tools/recover"))
        .bearer_auth("dev-token")
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    assert_eq!(r.status(), 503);

    // Wait past cooldown (200ms in test config).
    tokio::time::sleep(Duration::from_millis(300)).await;

    let r = client
        .post(proc.rest_url("/v1/tools/recover"))
        .bearer_auth("dev-token")
        .json(&json!({ "after": "cooldown" }))
        .send()
        .await
        .expect("POST");
    assert!(
        r.status().is_success(),
        "probe should have succeeded post-cooldown, got {}",
        r.status()
    );
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
                "audit line not found within {deadline:?}\nstdout:\n{}",
                proc.stdout_snapshot().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Keep Arc imports honest.
#[allow(dead_code)]
fn _u(_: Arc<()>) {}
