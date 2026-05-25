//! v0.2 PR 31 — WhatsApp Cloud API adapter (text-only inbound + outbound).
//!
//! Five scenarios:
//!
//! 1. `webhook_verify_handshake_echoes_challenge` — Meta's
//!    subscription probe. GET with matching `hub.mode=subscribe`
//!    and `hub.verify_token` returns 200 + the `hub.challenge`
//!    body verbatim. No HMAC on the GET (it's the setup step).
//! 2. `webhook_verify_handshake_rejects_wrong_token` — wrong
//!    verify_token → 403.
//! 3. `signed_message_dispatches_and_couriers` — POST a real
//!    Meta-shaped envelope signed with the app secret. We expect:
//!    200 ack, `phase: dispatch` audit, FakeWhatsAppApi captures
//!    one POST to `/v18.0/{phone_number_id}/messages` with the
//!    expected `Authorization: Bearer ...` header and body,
//!    `phase: post` audit with status_label=posted.
//! 4. `forged_signature_is_rejected` — POST with sha256=000... →
//!    401 + `phase: rejected, result: error:auth`.
//! 5. `unknown_sender_rejected` — correctly-signed body whose
//!    `from` isn't in the sender_table → 401 + error:auth.
//!
//! No mocks: real binary, real HTTP, real HMAC.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const VERIFY_TOKEN: &str = "meta-verify-token-for-test";
const ACCESS_TOKEN: &str = "whatsapp-access-token-for-test";
const PHONE_NUMBER_ID: &str = "100200300";
const KNOWN_WA_ID: &str = "491701234567";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-test.yaml")
        .display()
        .to_string()
}

fn whatsapp_inbound(wa_id: &str, text: &str) -> Value {
    // Minimal real Cloud API envelope: object/entry/changes/value/messages.
    json!({
        "object": "whatsapp_business_account",
        "entry": [{
            "id": "0",
            "changes": [{
                "value": {
                    "messaging_product": "whatsapp",
                    "metadata": {
                        "display_phone_number": "15555555555",
                        "phone_number_id": PHONE_NUMBER_ID,
                    },
                    "contacts": [{
                        "profile": { "name": "Alice" },
                        "wa_id": wa_id,
                    }],
                    "messages": [{
                        "from": wa_id,
                        "id": "wamid.HBgM",
                        "timestamp": "1700000000",
                        "type": "text",
                        "text": { "body": text }
                    }]
                },
                "field": "messages"
            }]
        }]
    })
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    let out = mac.finalize().into_bytes();
    format!("sha256={}", hex::encode(out))
}

fn env_with(whatsapp: &FakeWhatsAppApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_verify_handshake_echoes_challenge() {
    // Meta's one-time GET handshake: subscribe + matching token →
    // echo `hub.challenge`. NO HMAC on this path — it's the setup
    // ritual before the app has any messages to sign.
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .get(format!("http://{webhook_addr}/whatsapp/webhook"))
        .query(&[
            ("hub.mode", "subscribe"),
            ("hub.verify_token", VERIFY_TOKEN),
            ("hub.challenge", "the-magic-challenge"),
        ])
        .send()
        .await
        .expect("GET verify");
    assert_eq!(resp.status(), 200, "expected 200; got {}", resp.status());
    let body = resp.text().await.expect("body");
    assert_eq!(body, "the-magic-challenge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_verify_handshake_rejects_wrong_token() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .get(format!("http://{webhook_addr}/whatsapp/webhook"))
        .query(&[
            ("hub.mode", "subscribe"),
            ("hub.verify_token", "wrong-token"),
            ("hub.challenge", "should-not-echo"),
        ])
        .send()
        .await
        .expect("GET verify");
    assert_eq!(resp.status(), 403);
    let body = resp.text().await.unwrap_or_default();
    assert_ne!(body, "should-not-echo");

    // Per ADR-6 every refused inbound MUST flow through the
    // dispatcher's audit pivot.
    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_message_dispatches_and_couriers() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // Command parser routes `/echo <rest>` exactly like the
    // bare-text path — `echo <rest>` would also work, but we
    // exercise the `/<tool>` shape so the tests cover Telegram's
    // route_command idiom on this adapter too. Plain "hello world"
    // also routes to `echo` (the fall-through branch). Pick the
    // bare path so we don't accidentally couple the test to any
    // tool that doesn't exist in the default registry.
    let envelope = whatsapp_inbound(KNOWN_WA_ID, "hello world");
    let body = serde_json::to_vec(&envelope).expect("json");
    let sig = sign(&body, APP_SECRET);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    // 1. Dispatch audit.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    // 2. Outbound courier captured exactly one POST.
    let captured = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected exactly one outbound POST");
    let sent = &captured[0];
    assert_eq!(sent.phone_number_id, PHONE_NUMBER_ID);
    assert_eq!(
        sent.authorization,
        format!("Bearer {ACCESS_TOKEN}"),
        "bearer token must come from the resolved outbound.token credential",
    );
    assert_eq!(sent.body["messaging_product"], "whatsapp");
    assert_eq!(sent.body["recipient_type"], "individual");
    assert_eq!(sent.body["to"], KNOWN_WA_ID);
    assert_eq!(sent.body["type"], "text");
    assert_eq!(sent.body["text"]["preview_url"], false);
    let text = sent.body["text"]["body"].as_str().expect("body is string");
    assert!(
        text.contains("hello world"),
        "post-back text should embed the echoed message, got: {text}",
    );

    // 3. Post-back audit.
    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(post_audit["tool"], "echo");
    assert_eq!(post_audit["who"], "alice");
    assert_eq!(post_audit["result"], "ok");
    assert_eq!(post_audit["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_signature_is_rejected() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let envelope = whatsapp_inbound(KNOWN_WA_ID, "should never reach the dispatcher");
    let body = serde_json::to_vec(&envelope).expect("json");
    let forged = format!("sha256={}", "0".repeat(64));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", forged)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");

    // Belt-and-braces: a forged inbound MUST NOT trigger any
    // outbound courier traffic — otherwise an attacker could DoS
    // the bot's egress quota by spraying signed-looking-but-bad
    // payloads.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        whatsapp.captured().is_empty(),
        "forged inbound triggered outbound POST: {:?}",
        whatsapp.captured(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // wa_id 999... is correctly signed but NOT in the sender_table.
    let envelope = whatsapp_inbound("99999999999", "who am I");
    let body = serde_json::to_vec(&envelope).expect("json");
    let sig = sign(&body, APP_SECRET);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");
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

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe did not return Some within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
