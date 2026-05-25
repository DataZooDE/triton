//! v0.2 — Telegram `long_poll` inbound transport (FR-A-1.v0.2).
//!
//! Unlike the webhook adapter, the long-poll worker pulls updates
//! from `getUpdates` and feeds each through the same dispatch +
//! courier pipeline. The test seeds the fake Telegram API with one
//! update and asserts the worker (with no webhook POST from the
//! test) dispatches it and posts the reply back.
//!
//! No mocks per CLAUDE.md §1: real binary, real long-poll worker,
//! real HTTP against the `FakeTelegramApi` getUpdates + sendMessage
//! routes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-longpoll-test.yaml")
        .display()
        .to_string()
}

fn update(update_id: i64, user_id: u64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "Alice" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_poll_worker_fetches_dispatches_and_replies() {
    // Seed one update from the known sender (id 42 → alice/acme).
    let telegram = FakeTelegramApi::with_updates(vec![update(1, 42, "echo via longpoll")]).await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        // Poll promptly in the test instead of the 25s production hold.
        (
            "TRITON_TELEGRAM_LONGPOLL_TIMEOUT_SECS".to_string(),
            "0".to_string(),
        ),
        (
            "TRITON_TELEGRAM_LONGPOLL_BACKOFF_MS".to_string(),
            "50".to_string(),
        ),
    ]);
    // No webhook is mounted for a long_poll adapter, so we don't read
    // proc.chat_webhook_addr — the worker drives the whole flow.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // 1. The worker polled getUpdates, got the update, and dispatched
    //    it through the same pipeline as the webhook path.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(4), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    // 2. The reply was posted back to Telegram via the courier.
    let captured = wait_for(Duration::from_secs(4), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected exactly one sendMessage");
    assert_eq!(captured[0].body["chat_id"], 42);
    let text = captured[0].body["text"].as_str().expect("text");
    assert!(
        text.contains("echo via longpoll"),
        "reply should echo the inbound text; got: {text}"
    );

    // 3. Post-back audited as phase: post / posted.
    let post = wait_for_audit(&proc, Duration::from_secs(4), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(post["status_label"], "posted");
    assert_eq!(post["result"], "ok");
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
