//! Manifest-configurable inbound tool per chat adapter.
//!
//! Historically every chat adapter hardcoded "plain text → in-process
//! `echo`", and `build_registry` registered `echo` unconditionally
//! while the dispatcher prefers in-process tools over upstreams — so
//! a chat inbound could NEVER reach an upstream agent. Two changes
//! under test:
//!
//!   1. The adapter manifest's optional `tool` field names the tool
//!      plain chat text dispatches to (default `echo`; commands like
//!      `/narrate` keep their special routes).
//!   2. An in-process tool whose name is claimed by
//!      `TRITON_STATIC_UPSTREAMS` is skipped at registration so the
//!      dispatch falls through to the upstream router (see
//!      `static_upstream.rs` for the REST-side proof).
//!
//! No mocks: real binary, real HMAC-signed webhook, real HTTP to the
//! in-repo `FakeAgent` (upstream) and `FakeWhatsAppApi` (courier).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::upstream_fixture::FakeAgent;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const KNOWN_WA_ID: &str = "491701234567";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-cloud-inbound-tool.yaml")
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

async fn post_inbound(proc: &TritonProcess, wa_id: &str, text: &str) {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let body = serde_json::to_vec(&inbound_envelope(wa_id, text)).unwrap();
    let sig = sign(&body, APP_SECRET);
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook");
    assert!(resp.status().is_success(), "{}", resp.status());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_text_dispatches_to_manifest_configured_upstream_tool() {
    // A real upstream agent answers tool `assistant` via the static
    // upstream map (no Consul/Vault). The manifest sets
    // `tool: assistant`, so plain inbound text must reach the agent
    // — not the in-process `echo`.
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    post_inbound(&proc, KNOWN_WA_ID, "hello there").await;

    // The upstream agent received the adapter's plain-text args.
    let bodies = wait_for(Duration::from_secs(3), || {
        let v = agent.bodies_seen();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(
        bodies[0],
        json!({ "message": "hello there" }),
        "upstream agent must receive the plain-text args"
    );

    // And the agent's reply was couriered back to WhatsApp.
    let sent = wait_for(Duration::from_secs(3), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(sent[0].body["to"], KNOWN_WA_ID);
    assert_eq!(sent[0].body["type"], "text");
    let text = sent[0].body["text"]["body"].as_str().unwrap_or_default();
    assert!(
        text.contains("hello there"),
        "reply must carry the agent's echoed text; got: {text}"
    );

    // The dispatch was audited under the configured tool name.
    let dispatch = wait_for(Duration::from_secs(2), || {
        proc.stdout_snapshot()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| {
                v["kind"] == "audit"
                    && v["phase"] == "dispatch"
                    && v["protocol"] == "messenger:whatsapp"
            })
    });
    assert_eq!(dispatch["tool"], "assistant");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["result"], "ok");
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
