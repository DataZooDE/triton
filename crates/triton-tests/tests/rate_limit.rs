//! v0.2 PR 24 — manifest rate_limit enforcement (NFR-P-3).
//!
//! The Telegram adapter manifest declares
//! `rate_limit: { messages_per_sec: 1, burst: 2 }`. We send the
//! authenticated webhook 4 times back-to-back; the first 2 should
//! succeed (consuming the burst) and the next 2 should 429 +
//! audit as `phase: rejected, result: error:ratelimit`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;

const RESOLVED_SECRET: &str = "webhook-secret-for-test";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-rate-limit-test.yaml")
        .display()
        .to_string()
}

fn telegram_update(text: &str) -> Value {
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
            "chat": { "id": 42, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_succeeds_then_excess_is_ratelimited() {
    let telegram = FakeTelegramApi::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook_addr = proc.chat_webhook_addr.expect("listener bound");

    let client = reqwest::Client::new();
    let send = |text: &str| {
        let text = text.to_string();
        let url = format!("http://{webhook_addr}/telegram/webhook");
        let client = client.clone();
        async move {
            client
                .post(&url)
                .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
                .json(&telegram_update(&text))
                .send()
                .await
                .expect("POST")
                .status()
                .as_u16()
        }
    };

    // Burst = 2: first two messages must be admitted.
    assert_eq!(send("one").await, 200);
    assert_eq!(send("two").await, 200);

    // Third and fourth in quick succession should be rejected with
    // 429 — the bucket is empty and we haven't waited for refill.
    assert_eq!(send("three").await, 429);
    assert_eq!(send("four").await, 429);

    // Audit confirms the rate-limit class is in the pivot.
    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:ratelimit"
            && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(rejected["status"], 429);

    // FakeTelegramApi must have only seen the captures from the
    // two admitted dispatches. The rate-limited inbounds never
    // reach the dispatcher → no courier call.
    let count = telegram.captured().len();
    assert_eq!(
        count, 2,
        "expected 2 admitted dispatches → 2 sendMessage calls; saw {count}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_tenant_buckets_do_not_starve_each_other() {
    // PR 28 (NFR-P-3 second tier): two tenants sharing one
    // adapter MUST NOT block each other. Manifest: rate=1/sec,
    // burst=2 per tenant; adapter-wide bucket headroomed 10x so
    // it stays out of the way. Send 4 messages from each tenant
    // back-to-back; expect 200/200/429/429 PER TENANT.
    let telegram = FakeTelegramApi::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/manifest-per-tenant-rate-limit.yaml")
                .display()
                .to_string(),
        ),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook_addr = proc.chat_webhook_addr.expect("listener bound");

    let client = reqwest::Client::new();
    let send = |sender_id: u64, text: &str| {
        let text = text.to_string();
        let url = format!("http://{webhook_addr}/telegram/webhook");
        let client = client.clone();
        async move {
            let update = json!({
                "update_id": 100,
                "message": {
                    "message_id": 1,
                    "from": { "id": sender_id, "is_bot": false, "first_name": "X" },
                    "chat": { "id": sender_id, "type": "private" },
                    "date": 1_700_000_000,
                    "text": text
                }
            });
            client
                .post(&url)
                .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
                .json(&update)
                .send()
                .await
                .expect("POST")
                .status()
                .as_u16()
        }
    };

    // Tenant alpha (user 42) — first 2 admit, next 2 rate-limited.
    assert_eq!(send(42, "a1").await, 200);
    assert_eq!(send(42, "a2").await, 200);
    assert_eq!(send(42, "a3").await, 429);
    assert_eq!(send(42, "a4").await, 429);

    // Tenant beta (user 43) — separate bucket, still gets 2.
    // If per-tenant wasn't a thing, the adapter-wide bucket would
    // have been depleted (or would gate this one); the 10x
    // headroom on the adapter-wide bucket guarantees beta's
    // capacity is independent.
    assert_eq!(send(43, "b1").await, 200);
    assert_eq!(send(43, "b2").await, 200);
    assert_eq!(send(43, "b3").await, 429);

    // Audit emits one rate-limit line per tenant.
    let lines: Vec<Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "rejected"
                && v["result"] == "error:ratelimit"
                && v["protocol"] == "messenger:telegram"
        })
        .collect();
    // Alpha bumped twice (a3, a4); beta once (b3).
    assert!(
        lines.len() >= 3,
        "expected at least 3 rate-limit audit lines; got {}: {:?}",
        lines.len(),
        lines
    );
    let alpha_rejected = lines.iter().filter(|v| v["tenant"] == "alpha").count();
    let beta_rejected = lines.iter().filter(|v| v["tenant"] == "beta").count();
    assert!(
        alpha_rejected >= 2 && beta_rejected >= 1,
        "expected per-tenant audit attribution; got alpha={alpha_rejected} beta={beta_rejected}"
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
