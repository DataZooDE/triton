//! `chat inbound → real Triton → real adk-hello-agent resolver tool`
//! (issue #101 part B / FR-I-7, the `upstream` identity-resolution
//! strategy).
//!
//! The WhatsApp Cloud adapter declares `identity.kind: upstream` with
//! `resolver_tool: resolve_identity`. On an inbound from a sender no
//! `sender_table` knows, Triton dispatches `resolve_identity` to the
//! agent with `{platform, sender}`, expects `{sub, scopes, tenant}`
//! back, then dispatches the command tool (`hello`) AS that resolved
//! principal and couriers the reply. A resolver that rejects ⇒ the
//! inbound is refused 401, with no command dispatch.
//!
//! No mocks (CLAUDE.md §1): real `triton` binary, the real
//! `adk-hello-agent` binary (serving BOTH `hello` and `resolve_identity`,
//! branching on `X-Triton-Tool`), reached via `TRITON_STATIC_UPSTREAMS`,
//! with the in-repo `FakeWhatsAppApi` Cloud-API double for the outbound
//! leg. Real HMAC on the inbound webhook.
//!
//! Prereq: the `triton` binary must be built in the parent repo
//! (`cargo build -p triton-bin` at the Triton root).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
/// A sender no operator enumerated — only the resolver can name it.
const UNKNOWN_WA_ID: &str = "491701234567";
/// The resolver refuses any sender prefixed `blocked` (see src/main.rs).
const BLOCKED_WA_ID: &str = "blocked-490000000009";

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

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/manifest-resolver-e2e.yaml")
        .display()
        .to_string()
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn inbound_envelope(wa_id: &str, text: &str) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": "100200300" },
            "messages": [{ "from": wa_id, "id": "wamid.X", "timestamp": "1700000000",
                "type": "text", "text": { "body": text } }]
        }, "field": "messages" }] }]
    })
}

/// One real agent serves both the resolver tool and the command tool;
/// Triton routes both names to it via `TRITON_STATIC_UPSTREAMS`.
fn env_for(agent: &HelloAgent, whatsapp: &FakeWhatsAppApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("hello={a},resolve_identity={a}", a = agent.host_port()),
        ),
    ])
}

async fn post_inbound(proc: &TritonProcess, wa_id: &str, text: &str) -> reqwest::Response {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let body = serde_json::to_vec(&inbound_envelope(wa_id, text)).unwrap();
    let sig = sign(&body, APP_SECRET);
    reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_tool_resolves_unknown_sender_then_dispatches_and_couriers() {
    let agent = HelloAgent::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(10), env_for(&agent, &whatsapp)).await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hello via resolver").await;
    assert!(
        resp.status().is_success(),
        "inbound should be accepted; got {}",
        resp.status()
    );

    // The resolver tool ran (its own dedicated audit protocol label).
    let resolve = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:whatsapp:identity"
    });
    assert_eq!(resolve["tool"], "resolve_identity");
    assert_eq!(resolve["result"], "ok");

    // The command dispatch carries the resolver-returned principal.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:whatsapp"
            && v["tool"] == "hello"
    });
    assert_eq!(
        dispatch["who"],
        format!("wa:{UNKNOWN_WA_ID}"),
        "sub comes from the agent's resolver tool"
    );
    assert_eq!(dispatch["tenant"], "demo", "tenant from the resolver tool");
    assert_eq!(dispatch["result"], "ok");

    // The greeting was couriered back to the platform recipient.
    let sent = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(sent[0].body["to"], UNKNOWN_WA_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_rejection_refuses_the_inbound_401() {
    let agent = HelloAgent::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(10), env_for(&agent, &whatsapp)).await;

    let resp = post_inbound(&proc, BLOCKED_WA_ID, "should be refused").await;
    assert_eq!(
        resp.status(),
        401,
        "resolver rejection must refuse the inbound"
    );

    // No command dispatch, nothing couriered.
    std::thread::sleep(Duration::from_millis(400));
    let hello_dispatches = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:whatsapp"
                && v["tool"] == "hello"
        })
        .count();
    assert_eq!(
        hello_dispatches, 0,
        "no command dispatch on resolver rejection"
    );
    assert!(
        whatsapp.captured().is_empty(),
        "nothing couriered on a rejected inbound"
    );
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe timed out after {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn wait_for_audit<F>(proc: &TritonProcess, deadline: Duration, mut matches: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
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
