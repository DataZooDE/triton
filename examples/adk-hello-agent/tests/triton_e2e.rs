//! `frontend → real Triton → real adk-hello-agent`, across all three
//! HTTP frontends (REST, MCP, A2A).
//!
//! No mocks (CLAUDE.md §1): this boots the **real** `triton` binary and
//! the **real** `adk-hello-agent` binary over real TCP. The agent is
//! reached via Triton's static upstream map (`TRITON_STATIC_UPSTREAMS`)
//! — no Consul, no Vault. The agent runs its deterministic `StaticBrain`
//! (no `ANTHROPIC_API_KEY`), so the greeting is fixed and assertable
//! here; the live LLM path is covered by `live_llm.rs` (ignored by
//! default).
//!
//! Auth note: the agent verifies Triton's per-call bearer. With no OIDC
//! signer configured Triton dispatches in dev-token mode, forwarding the
//! literal `dev-token`, which the agent accepts. In production Triton
//! mints a per-call RS256 JWT the agent verifies against Triton's JWKS;
//! here we prove the agent is reachable and renders correctly through
//! every frontend.
//!
//! Prereq: the `triton` binary must be built in the parent repo
//! (`cargo build -p triton-bin` at the Triton root), and this crate's
//! own binary is built by cargo for the integration test.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use triton_tests::TritonProcess;

/// The real agent binary, spawned on a free loopback port with the
/// deterministic brain. `Drop` kills it.
struct HelloAgent {
    child: Child,
    port: u16,
}

impl HelloAgent {
    async fn start() -> Self {
        let port = free_port();
        let bin = env!("CARGO_BIN_EXE_adk-hello-agent");
        let child = Command::new(bin)
            .env("AGENT_PORT", port.to_string())
            // Force the deterministic StaticBrain and the dev-token auth
            // path (no issuer configured).
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("AGENT_OIDC_ISSUER")
            .env_remove("AGENT_OIDC_AUDIENCE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn adk-hello-agent");

        let client = reqwest::Client::new();
        let health = format!("http://127.0.0.1:{port}/healthz");
        let mut ok = false;
        for _ in 0..100 {
            if let Ok(r) = client.get(&health).send().await
                && r.status().is_success()
            {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ok, "agent /healthz never came up on port {port}");
        Self { child, port }
    }

    fn host_port(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for HelloAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind free port")
        .local_addr()
        .unwrap()
        .port()
}

/// Boot Triton wired to the live agent via the static upstream map.
/// The `hello` tool resolves to the agent's `host:port` — no Consul, no
/// Vault. With no OIDC signer configured Triton forwards the literal
/// `dev-token`, which the agent (dev-token mode) accepts.
async fn boot() -> (HelloAgent, TritonProcess) {
    let agent = HelloAgent::start().await;

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("hello={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    (agent, triton)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_reaches_the_agent() {
    let (_agent, triton) = boot().await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/hello"))
        .bearer_auth("dev-token")
        .json(&json!({ "subject": "Ada" }))
        .send()
        .await
        .expect("POST /v1/tools/hello");

    assert_eq!(resp.status(), 200, "REST should return 200");
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("Welcome to Triton") && body.contains("Ada"),
        "REST response should carry the greeting; got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_reaches_the_agent() {
    let (_agent, triton) = boot().await;

    let resp = reqwest::Client::new()
        .post(triton.mcp_url("/"))
        .bearer_auth("dev-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "hello",
                "arguments": { "subject": "Ada" },
                "_meta": { "a2ui_version": "v0.9" }
            }
        }))
        .send()
        .await
        .expect("POST / (MCP)");

    assert_eq!(resp.status(), 200, "MCP transport should return 200");
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("Welcome to Triton") && body.contains("Ada"),
        "MCP response should carry the greeting; got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_reaches_the_agent() {
    let (_agent, triton) = boot().await;

    let resp = reqwest::Client::new()
        .post(triton.a2a_url("/message:send"))
        .bearer_auth("dev-token")
        .json(&json!({
            "parts": [{ "data": { "tool": "hello", "args": { "subject": "Ada" } } }],
            "metadata": { "a2ui_version": "v0.8" }
        }))
        .send()
        .await
        .expect("POST /message:send (A2A)");

    assert_eq!(resp.status(), 200, "A2A should return 200");
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("Welcome to Triton") && body.contains("Ada"),
        "A2A response should carry the greeting; got: {body}"
    );
}
