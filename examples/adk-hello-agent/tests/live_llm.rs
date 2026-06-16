//! Live LLM smoke test — the genuine adk-rust `LlmAgent` path.
//!
//! Ignored by default (needs a real provider key + network). Run with:
//!
//!     ANTHROPIC_API_KEY=… cargo test --test live_llm -- --ignored
//!
//! Boots the real agent WITH the key so its adk-rust brain actually
//! calls Anthropic, then drives it through Triton's REST frontend. The
//! greeting is non-deterministic, so we only assert a healthy 200 and a
//! non-empty rendered surface — the deterministic content assertions
//! live in `triton_e2e.rs`.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use triton_tests::TritonProcess;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind free port")
        .local_addr()
        .unwrap()
        .port()
}

struct Agent {
    child: Child,
    port: u16,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs ANTHROPIC_API_KEY and network"]
async fn live_llm_greeting_through_rest() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set for the live test");

    let port = free_port();
    let bin = env!("CARGO_BIN_EXE_adk-hello-agent");
    let child = Command::new(bin)
        .env("AGENT_PORT", port.to_string())
        .env("ANTHROPIC_API_KEY", api_key)
        .env_remove("AGENT_OIDC_ISSUER")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn adk-hello-agent");
    let agent = Agent { child, port };

    let client = reqwest::Client::new();
    let health = format!("http://127.0.0.1:{}/healthz", agent.port);
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
    assert!(ok, "agent /healthz never came up");

    let env = HashMap::from([
        ("TRITON_ENV".into(), "nonprod".into()),
        // Reach the agent via the static upstream map — no Consul, no
        // Vault. With no OIDC signer Triton forwards the literal
        // `dev-token`, which the agent accepts.
        (
            "TRITON_STATIC_UPSTREAMS".into(),
            format!("hello=127.0.0.1:{}", agent.port),
        ),
        // Upstream LLM calls can be slow; give the dispatch room.
        ("TRITON_UPSTREAM_TIMEOUT".into(), "30s".into()),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/hello"))
        .bearer_auth("dev-token")
        .json(&json!({ "subject": "Ada Lovelace" }))
        .send()
        .await
        .expect("POST /v1/tools/hello");

    assert_eq!(resp.status(), 200, "live LLM call should return 200");
    let body = resp.text().await.expect("body");
    // A real greeting is non-deterministic; just prove a non-trivial
    // surface came back (narration + the Greet-again button).
    assert!(
        body.contains("narration") || body.contains("Greet again") || body.len() > 40,
        "expected a rendered greeting surface; got: {body}"
    );
}
