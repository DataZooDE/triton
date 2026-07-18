//! #191 PR-T2 — Twilio-WhatsApp adapter (text-only inbound + outbound).
//!
//! Twilio signs the exact externally-visible webhook URL, so this test
//! must know its own webhook port BEFORE spawning the binary (unlike
//! every other chat-adapter test, which reads `proc.chat_webhook_addr`
//! AFTER spawn). We pick the port ourselves via
//! `triton_tests::free_tcp_port()`, override `TRITON_CHAT_WEBHOOK_PORT`
//! to that value, and point the manifest's `public_url` at the matching
//! URL — see that function's doc comment for why `proc.chat_webhook_addr`
//! must NOT be used here.
//!
//! Four scenarios:
//! 1. `signed_message_dispatches_and_couriers` — POST a real Twilio-shaped
//!    form body signed with the Auth Token. Expect: 200 ack, `phase:
//!    dispatch` audit, `FakeTwilioApi` captures one POST to the Messages
//!    API with the expected HTTP Basic auth + form body, `phase: post`
//!    audit with status_label=posted.
//! 2. `forged_signature_is_rejected` — wrong `X-Twilio-Signature` → 401 +
//!    `phase: rejected, result: error:auth`, and no outbound POST fires.
//! 3. `unknown_sender_rejected` — correctly-signed body whose `From`
//!    isn't in the sender_table → 401 + error:auth.
//! 4. `missing_body_is_silently_acked` — a signed request with no `Body`
//!    (e.g. a status ping) → 200, no dispatch.
//!
//! No mocks: real binary, real HTTP, real HMAC-SHA1.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTwilioApi;

const AUTH_TOKEN: &str = "twilio-auth-token-for-test";
const ACCOUNT_SID: &str = "ACtest00000000000000000000000000";
const FROM_SENDER: &str = "whatsapp:+14155238886";
const KNOWN_WA_NUMBER: &str = "+491701234567";

fn manifest_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-twilio-whatsapp-test.yaml")
        .display()
        .to_string()
}

fn webhook_path() -> &'static str {
    "/twilio-whatsapp/webhook"
}

/// Env map for a run pinned to `port` (see module doc for why we must
/// pick this ourselves rather than read it off the spawned process).
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
    // Independent of the adapter's own `signature::verify` — this test
    // builds the header the way a real Twilio request would, using the
    // published algorithm directly, so a bug in the adapter's verify
    // path can't also hide in a shared helper.
    use base64::Engine;
    use hmac::{Hmac, Mac};
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
async fn signed_message_dispatches_and_couriers() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    let form = [
        ("MessageSid", "SM00000000000000000000000000000000"),
        ("From", "whatsapp:+491701234567"),
        ("To", FROM_SENDER),
        ("Body", "hello world"),
    ];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);

    let resp = post_webhook(&base_url, &form, &sig).await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    let captured = wait_for(Duration::from_secs(2), || {
        let v = twilio.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected exactly one outbound POST");
    let sent = &captured[0];
    assert_eq!(sent.account_sid_in_path, ACCOUNT_SID);
    let expected_auth = format!("Basic {}", {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(format!("{ACCOUNT_SID}:{AUTH_TOKEN}"))
    });
    assert_eq!(sent.authorization, expected_auth);
    assert_eq!(sent.form.get("From").map(String::as_str), Some(FROM_SENDER));
    assert_eq!(
        sent.form.get("To").map(String::as_str),
        Some(format!("whatsapp:{KNOWN_WA_NUMBER}").as_str())
    );
    let body = sent.form.get("Body").expect("Body present");
    assert!(
        body.contains("hello world"),
        "post-back text should embed the echoed message, got: {body}"
    );

    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(post_audit["tool"], "echo");
    assert_eq!(post_audit["result"], "ok");
    assert_eq!(post_audit["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_signature_is_rejected() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");

    let form = [
        ("From", "whatsapp:+491701234567"),
        ("To", FROM_SENDER),
        ("Body", "should never reach the dispatcher"),
    ];
    let forged = "not-the-real-signature==";

    let resp = post_webhook(&base_url, &form, forged).await;
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");

    std::thread::sleep(Duration::from_millis(250));
    assert!(
        twilio.captured().is_empty(),
        "forged inbound triggered outbound POST: {:?}",
        twilio.captured(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    let form = [
        ("From", "whatsapp:+19999999999"),
        ("To", FROM_SENDER),
        ("Body", "who am I"),
    ];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);

    let resp = post_webhook(&base_url, &form, &sig).await;
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_body_is_silently_acked() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    // A signed request with no `Body` — e.g. a delivery status ping
    // landing on the wrong route — must 200 and dispatch nothing.
    let form = [("From", "whatsapp:+491701234567"), ("To", FROM_SENDER)];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);

    let resp = post_webhook(&base_url, &form, &sig).await;
    assert_eq!(resp.status(), 200);

    std::thread::sleep(Duration::from_millis(250));
    assert!(twilio.captured().is_empty(), "no Body must not dispatch");
    let _ = proc;
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
