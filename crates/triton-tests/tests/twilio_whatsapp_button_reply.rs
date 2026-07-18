//! #191 PR-T4 — Twilio-WhatsApp inbound button-tap routing.
//!
//! Rescoped from the original plan after PR-T3's research: Twilio's
//! WhatsApp Quick Reply/List buttons live inside operator pre-authored
//! Content Templates (ContentSid), not something Triton builds
//! dynamically per message — so there is no signed correlation token to
//! decode the way Telegram's `callback_query` handler does (WhatsApp
//! Cloud's own `#94` correlation-token pattern doesn't apply here
//! either, for the same reason). When a user taps a button, Twilio's
//! inbound webhook instead carries `ButtonPayload` (the operator-defined
//! postback string baked into the template at authoring time) and
//! `ButtonText` — an ordinary inbound message, just with those two
//! extra fields alongside (often empty) `Body`.
//!
//! So "decoding an interactive reply" here means: prefer `ButtonPayload`
//! over `Body` as the routing text when present, and dispatch through
//! the SAME `route_command` pipeline a typed message already uses — the
//! operator authors the template's payload strings to look like
//! commands (e.g. `/confirm 123`) if they want structured routing.
//!
//! No mocks: real binary, real HTTP, real HMAC-SHA1.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTwilioApi;

const AUTH_TOKEN: &str = "twilio-auth-token-for-test";
const FROM_SENDER: &str = "whatsapp:+14155238886";

fn manifest_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-twilio-whatsapp-test.yaml")
        .display()
        .to_string()
}

fn webhook_path() -> &'static str {
    "/twilio-whatsapp/webhook"
}

fn env_with(port: u16, twilio: &FakeTwilioApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TWILIO_API_BASE".to_string(), twilio.url()),
        ("TRITON_CHAT_WEBHOOK_PORT".to_string(), port.to_string()),
        (
            "TRITON_TWILIO_WHATSAPP_PUBLIC_URL".to_string(),
            format!("http://127.0.0.1:{port}{}", webhook_path()),
        ),
    ])
}

fn sign(url: &str, form: &[(&str, &str)], auth_token: &str) -> String {
    let mut sorted: Vec<&(&str, &str)> = form.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut s = String::from(url);
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(auth_token.as_bytes()).expect("hmac key");
    mac.update(s.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

async fn post_webhook(base_url: &str, form: &[(&str, &str)], signature: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base_url}{}", webhook_path()))
        .header("X-Twilio-Signature", signature)
        .form(form)
        .send()
        .await
        .expect("POST webhook")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn button_payload_routes_like_typed_text() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    // A real Quick Reply tap: Body is empty (or a copy of ButtonText,
    // depending on Twilio's channel version), ButtonPayload carries the
    // operator-authored postback string, ButtonText the visible label.
    let form = [
        ("MessageSid", "SM00000000000000000000000000000001"),
        ("From", "whatsapp:+491701234567"),
        ("To", FROM_SENDER),
        ("Body", ""),
        ("ButtonPayload", "hello from a button"),
        ("ButtonText", "Say hi"),
        ("ButtonType", "REPLY"),
    ];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);

    let resp = post_webhook(&base_url, &form, &sig).await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(
        dispatch["tool"], "echo",
        "button tap must dispatch through the same route as typed text"
    );
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["result"], "ok");

    let captured = wait_for(Duration::from_secs(2), || {
        let v = twilio.captured();
        (!v.is_empty()).then_some(v)
    });
    let body = captured[0].form.get("Body").expect("Body present");
    assert!(
        body.contains("hello from a button"),
        "post-back should echo the ButtonPayload text, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_button_payload_falls_back_to_body() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    // A normal text message carries no ButtonPayload field at all — must
    // still route on Body exactly as before (regression guard).
    let form = [
        ("From", "whatsapp:+491701234567"),
        ("To", FROM_SENDER),
        ("Body", "plain text, no button"),
    ];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);
    let resp = post_webhook(&base_url, &form, &sig).await;
    assert!(resp.status().is_success());

    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(dispatch["tool"], "echo");
    let _ = proc;
    let _ = twilio;
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
