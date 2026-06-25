//! Mode 2 (issue #75): `TRITON_STATIC_UPSTREAMS` lets one real `triton`
//! binary front a single agent endpoint with **no Consul, no Vault**.
//! No mocks: a real triton + a real FakeAgent over real HTTP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_upstream_dispatch_no_hashicorp() {
    let agent = FakeAgent::start_echoing().await;

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        // No TRITON_CONSUL_URL / TRITON_VAULT_URL — just a static map.
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let http = reqwest::Client::new();

    // The agent isn't in the in-process registry, but /v1/tools lists it
    // (StaticUpstream::list_agents) flagged upstream.
    let tools: Value = http
        .get(triton.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("json");
    let listed = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "assistant")
        .expect("assistant listed");
    assert_eq!(listed["upstream"], json!(true));

    // And it dispatches to the static endpoint.
    let resp: Value = http
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": "static-42" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant")
        .json()
        .await
        .expect("json");
    assert_eq!(
        resp["result"]["echoed"]["marker"], "static-42",
        "static upstream echoed the args: {resp}"
    );

    // The agent saw the static dev-token (no Vault swap happened).
    assert_eq!(agent.bearers_seen()[0], "dev-token");

    // Contract parity with the Consul-mode router (#101): static
    // dispatch carries the informational `X-Triton-Tool` header too,
    // so an agent serving several tools can route without parsing
    // the args body.
    assert_eq!(
        agent.tools_seen()[0].as_deref(),
        Some("assistant"),
        "static upstream dispatch must carry X-Triton-Tool"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_upstream_shadows_inprocess_tool() {
    // When TRITON_STATIC_UPSTREAMS names a tool that would also be
    // registered in-process (`echo`), the in-process registration is
    // skipped so dispatch falls through to the upstream router —
    // otherwise the static map entry would be silently unreachable
    // (the dispatcher prefers in-process tools).
    let agent = FakeAgent::start_echoing().await;

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("echo={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp: Value = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "shadow-1" }))
        .send()
        .await
        .expect("POST /v1/tools/echo")
        .json()
        .await
        .expect("json");
    // The upstream FakeAgent wraps the args in `echoed`; the
    // in-process tool would have answered `{"echo": "shadow-1"}`.
    assert_eq!(
        resp["result"]["echoed"]["message"], "shadow-1",
        "echo must dispatch to the static upstream, not in-process: {resp}"
    );
    assert_eq!(agent.hits(), 1, "the upstream agent must be hit");

    // Boot logged the shadowing decision.
    assert!(
        triton
            .stdout_snapshot()
            .iter()
            .any(|l| l.contains("shadowed by static upstream")),
        "expected an info line about the skipped in-process tool"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sick_static_upstream_trips_the_per_tool_circuit_breaker() {
    // FR-U-3/4: the per-tool breaker survived the Consul/Vault decommission.
    // A sick agent that 500s every call trips the breaker after
    // TRITON_CIRCUIT_OPEN_AFTER faults; further calls fail fast with 503
    // (circuit_open) WITHOUT touching the agent, instead of every caller
    // waiting out the per-call timeout.
    let agent = FakeAgent::start_always_failing().await;
    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("flaky={}", agent.host_port()),
        ),
        ("TRITON_CIRCUIT_OPEN_AFTER".into(), "2".into()),
        // Long cooldown so the breaker stays open for the whole test.
        ("TRITON_CIRCUIT_COOLDOWN_MS".into(), "60000".into()),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let http = reqwest::Client::new();

    let call = || async {
        http.post(triton.rest_url("/v1/tools/flaky"))
            .bearer_auth("dev-token")
            .json(&json!({ "x": 1 }))
            .send()
            .await
            .expect("POST")
            .status()
            .as_u16()
    };

    // First two calls reach the agent and fail (502 — agent-side fault).
    assert_eq!(call().await, 502, "1st failing dispatch");
    assert_eq!(call().await, 502, "2nd failing dispatch trips the breaker");
    // Breaker now open: subsequent calls fail fast with 503 and never reach
    // the agent.
    assert_eq!(call().await, 503, "breaker open → fail fast");
    assert_eq!(call().await, 503, "still open within cooldown");
    assert_eq!(
        agent.hits(),
        2,
        "open breaker must short-circuit before dialling the agent"
    );
}

/// Locate the `triton` binary cargo built for these integration tests.
fn triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        for cand in ["target/debug/triton", "target/release/triton"] {
            let p = here.join(cand);
            if p.exists() {
                return p;
            }
        }
        here.pop();
    }
    panic!("could not locate `triton` binary; run `cargo build` first");
}

#[test]
fn nonlocal_env_refuses_a_public_static_upstream_endpoint() {
    // NFR-S-4 SSRF guard: outside `local` env a `TRITON_STATIC_UPSTREAMS`
    // endpoint pointing at a public/metadata host must fail boot closed,
    // never dial it with a minted agent bearer.
    let out = std::process::Command::new(triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "nonprod")
        .env("TRITON_STATIC_UPSTREAMS", "evil=1.2.3.4:80")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a public static-upstream endpoint MUST refuse to boot under nonprod; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn nonlocal_env_refuses_a_userinfo_smuggled_endpoint() {
    // TS-01: an allowed `.ts.net` suffix in URL userinfo must not slip past
    // the SSRF guard — reqwest would treat `carl.ts.net:x` as userinfo and
    // connect to the metadata IP after `@`, leaking the minted bearer. The
    // boot guard must refuse it just like a bare public endpoint.
    let out = std::process::Command::new(triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "nonprod")
        .env(
            "TRITON_STATIC_UPSTREAMS",
            "evil=carl.ts.net:8001@169.254.169.254",
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a userinfo-smuggled static-upstream endpoint MUST refuse to boot under nonprod; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}
