//! v0.2 PR 18 — outbound Telegram courier.
//!
//! After the dispatcher returns a result, the adapter POSTs the
//! result back to `api.telegram.org/bot{token}/sendMessage`. PR 18
//! ships the bare-text variant: the body is `{chat_id, text:
//! <result_as_json_string>}`. The L6' surface mapper (PR 19) turns
//! A2UI envelopes into native Telegram messages with inline
//! keyboards etc.; this PR is the wire.
//!
//! The post-back is audited as `phase: post` so the audit
//! collector sees the full inbound→dispatch→post cycle. Per ADR-6
//! the dispatcher is the single audit pivot, so the adapter calls
//! a new `Dispatcher::record_post` rather than emitting itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";
const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

fn telegram_update(user_id: u64, text: &str) -> Value {
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "Alice" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

async fn start_kv_vault() -> FakeVault {
    FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/telegram",
            &[
                ("webhook_secret", RESOLVED_SECRET),
                ("bot_token", BOT_TOKEN),
                (
                    "senders",
                    r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#,
                ),
                ("correlation_key", "correlation-key-from-vault"),
            ],
        )],
    )
    .await
}

fn env_with(vault: &FakeVault, telegram: &FakeTelegramApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_result_is_posted_back_to_telegram_and_audited() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update(42, "echo me back"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "webhook returned {}",
        resp.status()
    );

    // 1. Dispatcher audit fires first.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["result"], "ok");

    // 2. Adapter posts the result back to api.telegram.org.
    let captured = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected exactly one sendMessage");
    let sent = &captured[0];
    assert_eq!(
        sent.token, BOT_TOKEN,
        "courier must use the resolved bot token in the URL path",
    );
    assert_eq!(
        sent.body["chat_id"], 42,
        "chat_id must come from the inbound update's `message.from.id`",
    );
    let text = sent.body["text"].as_str().expect("text is a string");
    assert!(
        text.contains("echo me back"),
        "post-back text should embed the tool result, got: {text}"
    );

    // 3. Post-back audit fires with phase=post and result=ok.
    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(post_audit["tool"], "echo");
    assert_eq!(post_audit["who"], "alice");
    assert_eq!(post_audit["result"], "ok");
    assert_eq!(post_audit["status"], 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_failure_audits_provider_error_does_not_fail_inbound_ack() {
    // The Telegram API is unreachable but the dispatcher still
    // ran successfully — the inbound webhook MUST still return
    // 200 (we acked the message; the message-was-handled half is
    // independent of whether we managed to ship the reply). The
    // courier failure is audited as `phase: post, result: error:provider`.
    let vault = start_kv_vault().await;
    let proc = {
        let mut env = HashMap::from([
            ("TRITON_ENV".to_string(), "local".to_string()),
            ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
            ("TRITON_VAULT_URL".to_string(), vault.url()),
            ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
            // Port 1 is reserved; nothing listens there.
            (
                "TRITON_TELEGRAM_API_BASE".to_string(),
                "http://127.0.0.1:1".to_string(),
            ),
        ]);
        // Tight courier timeout so the test doesn't sit on a
        // connection-refused-but-retried path.
        env.insert("TRITON_COURIER_TIMEOUT_MS".to_string(), "500".to_string());
        TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
    };
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update(42, "hi"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "inbound must still ack 200 when post-back fails: got {}",
        resp.status()
    );

    let post_audit = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(post_audit["result"], "error:provider");
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
