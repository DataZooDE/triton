//! #94 — WhatsApp Cloud API interactive buttons / lists (outbound
//! rendering). A2UI `Button` components render as `interactive` reply
//! buttons; `Selection` renders as an `interactive` list. Each choice's
//! `id` is an HMAC correlation token (forward-compatible with a future
//! inbound `interactive`-reply handler). Inbound tap-routing is a
//! separate follow-up; this covers the surface-mapping richness only.
//!
//! Driven through the proactive endpoint (#95): the agent supplies an
//! A2UI surface as the outbound `result`, inside the recipient's 24-h
//! service window. No mocks — real binary, real OIDC, real HMAC inbound,
//! real HTTP to the in-repo `FakeWhatsAppApi`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::chat_courier_fixture::{FakeWhatsAppApi, WhatsAppSentMessage};
use triton_tests::{TestIssuer, TritonProcess};

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const KNOWN_WA_ID: &str = "491701234567";
const OUTBOUND_AUDIENCE: &str = "outbound-local";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-cloud-template.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(issuer: &TestIssuer, whatsapp: &FakeWhatsAppApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "agents-local".to_string(),
        ),
        (
            "TRITON_OUTBOUND_AUDIENCE".to_string(),
            OUTBOUND_AUDIENCE.to_string(),
        ),
    ])
}

fn outbound_token(issuer: &TestIssuer) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "carl-agent",
        "aud": OUTBOUND_AUDIENCE,
        "exp": now() + 60,
        "iat": now(),
        "tenant": "acme",
        "scope": "outbound:send",
    }))
}

async fn open_window(proc: &TritonProcess, wa_id: &str) {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let envelope = json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": "100200300" },
            "messages": [{ "from": wa_id, "id": "wamid.X", "timestamp": "1700000000",
                "type": "text", "text": { "body": "hi" } }]
        }, "field": "messages" }] }]
    });
    let body = serde_json::to_vec(&envelope).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(APP_SECRET.as_bytes()).expect("hmac key");
    mac.update(&body);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook");
    assert!(resp.status().is_success());
}

async fn send_surface(proc: &TritonProcess, issuer: &TestIssuer, surface: Value) {
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(issuer))
        .json(
            &json!({ "adapter": "whatsapp", "to": KNOWN_WA_ID, "result": { "surface": surface } }),
        )
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        202,
        "got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buttons_render_as_interactive_reply_buttons() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;
    open_window(&proc, KNOWN_WA_ID).await;

    send_surface(
        &proc,
        &issuer,
        json!({ "components": [
            { "kind": "text", "value": "Pick one:" },
            { "kind": "button", "label": "Yes", "tool": "narrate", "args": { "subject": "yes" } },
            { "kind": "button", "label": "No", "tool": "narrate", "args": { "subject": "no" } },
        ]}),
    )
    .await;

    let sent = wait_for(Duration::from_secs(2), || interactive_capture(&whatsapp));
    let i = &sent.body["interactive"];
    assert_eq!(sent.body["type"], "interactive");
    assert_eq!(i["type"], "button");
    assert_eq!(i["body"]["text"], "Pick one:");
    let buttons = i["action"]["buttons"].as_array().expect("buttons array");
    assert_eq!(buttons.len(), 2);
    assert_eq!(buttons[0]["type"], "reply");
    assert_eq!(buttons[0]["reply"]["title"], "Yes");
    assert_eq!(buttons[1]["reply"]["title"], "No");
    // ids carry a non-empty correlation token (the button's tool/args
    // MUST NOT leak as plain text the user could retype).
    assert!(
        buttons[0]["reply"]["id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "reply id is a correlation token"
    );
    let dump = serde_json::to_string(&sent.body).unwrap();
    assert!(!dump.contains("narrate"), "tool name must not leak: {dump}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_renders_as_interactive_list() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;
    open_window(&proc, KNOWN_WA_ID).await;

    send_surface(
        &proc,
        &issuer,
        json!({ "components": [
            { "kind": "text", "value": "Choose a fruit" },
            { "kind": "selection", "prompt": "Fruit",
              "options": [ { "label": "Apple", "value": "a" }, { "label": "Banana", "value": "b" } ],
              "tool": "narrate", "args_key": "fruit" },
        ]}),
    )
    .await;

    let sent = wait_for(Duration::from_secs(2), || interactive_capture(&whatsapp));
    let i = &sent.body["interactive"];
    assert_eq!(sent.body["type"], "interactive");
    assert_eq!(i["type"], "list");
    assert_eq!(i["body"]["text"], "Choose a fruit");
    assert!(
        i["action"]["button"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    let rows = i["action"]["sections"][0]["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["title"], "Apple");
    assert_eq!(rows[1]["title"], "Banana");
    assert!(rows[0]["id"].as_str().is_some_and(|s| !s.is_empty()));
}

fn interactive_capture(whatsapp: &FakeWhatsAppApi) -> Option<WhatsAppSentMessage> {
    whatsapp
        .captured()
        .into_iter()
        .find(|m| m.body["type"] == "interactive")
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
