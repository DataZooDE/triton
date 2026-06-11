//! #94 — WhatsApp Cloud API message templates.
//!
//! Outside the 24-hour service window a free-form text send is rejected
//! by Meta; proactive messages must use an approved **template**. Triton
//! owns template selection: the upstream agent supplies a `category`
//! hint + body `variables` via `POST /v1/outbound` (#95), and the
//! adapter resolves the Meta-approved template name+language from the
//! manifest's `templates` map and posts a `type: template` body.
//!
//! Three scenarios:
//!   1. category present, recipient outside the window → template send.
//!   2. no category, recipient inside the window (just messaged in) →
//!      free-form text send.
//!   3. no category, recipient outside the window → 400, nothing sent.
//!
//! No mocks: real binary, real OIDC issuer, real HMAC inbound, real HTTP
//! to the in-repo `FakeWhatsAppApi`.

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
    assert!(resp.status().is_success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn category_send_outside_window_uses_template() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    // No inbound first → the recipient is OUTSIDE the 24-h window.
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer))
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "category": "utility",
            "variables": ["Alice", "9am"],
        }))
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

    let sent = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    let body = &sent[0].body;
    assert_eq!(body["type"], "template", "expected a template send: {body}");
    assert_eq!(body["to"], KNOWN_WA_ID);
    // Triton resolved category `utility` → the manifest's template.
    assert_eq!(body["template"]["name"], "carl_reminder");
    assert_eq!(body["template"]["language"]["code"], "en");
    // The agent's variables become ordered body parameters.
    let params = &body["template"]["components"][0]["parameters"];
    assert_eq!(body["template"]["components"][0]["type"], "body");
    assert_eq!(params[0]["type"], "text");
    assert_eq!(params[0]["text"], "Alice");
    assert_eq!(params[1]["text"], "9am");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_category_inside_window_is_freeform() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    // Inbound message opens the 24-h service window (and couriers one
    // echo reply, which is the first capture).
    post_inbound(&proc, KNOWN_WA_ID, "hello").await;
    let _first = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });

    // No category, window open → free-form text send.
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer))
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "result": { "text": "free-form follow-up" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(resp.status(), 202);

    let outbound = wait_for(Duration::from_secs(2), || {
        let all = whatsapp.captured();
        last_freeform_to(&all, "free-form follow-up")
    });
    assert_eq!(outbound.body["type"], "text");
    assert_eq!(outbound.body["text"]["body"], "free-form follow-up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_category_outside_window_is_rejected() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    // No inbound → outside window; no category → cannot free-form.
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer))
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "result": { "text": "should be refused" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(resp.status(), 400);

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        whatsapp.captured().is_empty(),
        "a refused window-closed send must not reach the platform"
    );
}

fn last_freeform_to(all: &[WhatsAppSentMessage], body_text: &str) -> Option<WhatsAppSentMessage> {
    all.iter()
        .rev()
        .find(|m| m.body["text"]["body"] == body_text)
        .cloned()
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
