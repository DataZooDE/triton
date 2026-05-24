//! v0.2 PR 13 — Telegram inbound webhook + sender_table identity.
//!
//! Proves the inbound half end-to-end: a Telegram-shaped webhook
//! POST with the right `X-Telegram-Bot-Api-Secret-Token` and a
//! known sender_table entry reaches the dispatcher, fires the
//! `echo` tool, and emits one `phase: dispatch` audit line tagged
//! `protocol: messenger:telegram`. The outbound courier (posting
//! back to api.telegram.org) lands in PR 14.
//!
//! No mocks: real binary, real HTTP, real audit lines.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;

const SECRET: &str = "webhook-secret-for-test";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-test.yaml")
        .display()
        .to_string()
}

fn telegram_update(user_id: u64, text: &str) -> Value {
    // Minimal real Telegram `Update`. Triton's adapter parses
    // `message.from.id` for sender resolution and `message.text`
    // for the `echo` tool's `message` arg.
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": {
                "id": user_id,
                "is_bot": false,
                "first_name": "Alice",
            },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

fn env_with_manifest() -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_webhook_dispatches_echo_and_audits() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_manifest()).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .json(&telegram_update(42, "hello from telegram"))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["tenant"], "acme");
    assert_eq!(audit["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_secret_token_refused_and_audited() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_manifest()).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header(
            "X-Telegram-Bot-Api-Secret-Token",
            "definitely-not-the-secret",
        )
        .json(&telegram_update(42, "intruder"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_body_with_valid_secret_acked_and_audited_validation() {
    // Codex flagged in PR 13 review: the original handler used the
    // `Json(...)` extractor, which parses the body BEFORE the
    // handler runs. A garbage payload with the right secret would
    // be 400'd by axum without ever emitting a `phase: rejected`
    // audit line, and Telegram would retry the broken update for
    // ~24 h. Fix: read raw `Bytes`, verify the secret first, then
    // parse — and audit as `validation` + ack 400.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_manifest()).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .header("content-type", "application/json")
        .body("{ this is not valid json")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 400);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(rejected["result"], "error:validation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_refused_and_audited() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_manifest()).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        // user_id 9999 is NOT in the sender_table.
        .json(&telegram_update(9999, "who am I"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:telegram"
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
