//! v0.2 PR 33 — Google Chat adapter integration tests.
//!
//! Drives the full inbound flow against the real binary:
//!   * Google-signed JWT verification (RS256, real PEM cert served
//!     by `FakeGoogleJwks` on a real TCP port).
//!   * Sender resolution against the `sender_table`.
//!   * Inline response — Google Chat's synchronous-response pattern
//!     means the HTTP 200 body of the webhook IS the bot's reply.
//!   * Non-MESSAGE event types acked without dispatch.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real
//! RS256-signed JWT against a real PEM-wrapped X.509 cert.
//!
//! Fixture: `crates/triton-tests/src/chat_courier_fixture.rs`'s
//! `FakeGoogleJwks` ships a pre-generated RSA-2048 keypair so
//! `cargo test --workspace` is deterministic across runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::{FakeGoogleJwks, attacker_signing_key};

const AUDIENCE: &str = "1234567890";
const GOOGLE_ISS: &str = "chat@system.gserviceaccount.com";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-google-chat-test.yaml")
        .display()
        .to_string()
}

fn env_with(jwks: &FakeGoogleJwks) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
    ])
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn standard_claims() -> Value {
    let now = unix_now();
    json!({
        "iss": GOOGLE_ISS,
        "aud": AUDIENCE,
        "iat": now,
        "exp": now + 600,
        "sub": "google-chat-svc",
    })
}

/// Standard MESSAGE event body. Mirror of the Google Chat
/// developer-docs payload shape; only the fields the adapter
/// reads are populated.
fn message_event(sender_name: &str, text: &str) -> Value {
    json!({
        "type": "MESSAGE",
        "eventTime": "2026-05-25T10:00:00.000Z",
        "message": {
            "name": "spaces/AAA/messages/BBB",
            "sender": { "name": sender_name, "displayName": "Alice", "type": "HUMAN" },
            "text": text,
            "space": { "name": "spaces/AAA", "type": "DM" }
        },
        "space": { "name": "spaces/AAA", "type": "DM" },
        "user": { "name": sender_name }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_jwt_message_dispatches_inline_response() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "hello from google chat"))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    let body: Value = resp.json().await.expect("response body");
    // Echo just returns its `message` arg under the same key; the
    // adapter's bare-text path renders it as `text: "<msg>"`.
    let text = body["text"].as_str().expect("text in inline response");
    assert!(
        text.contains("hello from google chat"),
        "expected echo of the inbound text; got: {text}"
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["tenant"], "acme");
    assert_eq!(audit["result"], "ok");

    // The inline-response delivery is audited as `phase: post` with
    // status_label `posted` (latency_ms = 0 because there's no
    // outbound roundtrip).
    let post = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(post["status_label"], "posted");
    assert_eq!(post["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_jwt_signature_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Sign with a DIFFERENT RSA keypair; the fixture cert won't
    // verify against this signature.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    // Use the fixture's published kid so the keyset lookup hits
    // the served cert; the cert's public key won't match the
    // attacker's private key, so RSA verify fails.
    header.kid = Some("triton-test-google-chat-key".to_string());
    let forged = jsonwebtoken::encode(&header, &standard_claims(), &attacker_signing_key())
        .expect("attacker signs");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {forged}"))
        .json(&message_event("users/99", "intruder"))
        .send()
        .await
        .expect("POST forged");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_jwt_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // exp = 1 hour ago (well past the 5-minute leeway).
    let now = unix_now();
    let stale = json!({
        "iss": GOOGLE_ISS,
        "aud": AUDIENCE,
        "iat": now - 7200,
        "exp": now - 3600,
        "sub": "google-chat-svc",
    });
    let token = jwks.sign_jwt(stale);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "late"))
        .send()
        .await
        .expect("POST expired");
    assert_eq!(resp.status(), 401);
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:google_chat"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_audience_jwt_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let now = unix_now();
    let wrong_aud = json!({
        "iss": GOOGLE_ISS,
        "aud": "9999999999",
        "iat": now,
        "exp": now + 600,
        "sub": "google-chat-svc",
    });
    let token = jwks.sign_jwt(wrong_aud);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "wrong aud"))
        .send()
        .await
        .expect("POST wrong-aud");
    assert_eq!(resp.status(), 401);
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:google_chat"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        // users/77 is NOT in the sender_table.
        .json(&message_event("users/77", "who am I"))
        .send()
        .await
        .expect("POST unknown sender");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_message_event_types_silently_acked() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let body = json!({
        "type": "ADDED_TO_SPACE",
        "eventTime": "2026-05-25T10:00:00.000Z",
        "space": { "name": "spaces/AAA", "type": "DM" },
        "user": { "name": "users/99" }
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("POST added_to_space");
    assert!(resp.status().is_success(), "{}", resp.status());

    let resp_body: Value = resp.json().await.expect("response body");
    // Empty object body — no `text` for Google to deliver.
    assert!(
        resp_body.as_object().map(|o| o.is_empty()).unwrap_or(false),
        "non-MESSAGE event MUST ack with empty body; got: {resp_body}"
    );

    // Wait a moment for any audit lines to flush, then assert no
    // dispatch / no rejected for this protocol. (Other non-google
    // audit lines are fine.)
    std::thread::sleep(Duration::from_millis(200));
    let any_google: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "audit" && v["protocol"] == "messenger:google_chat")
        .count();
    assert_eq!(
        any_google, 0,
        "ADDED_TO_SPACE MUST NOT produce any messenger:google_chat audit lines",
    );
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
