//! v0.2 PR 21 — HMAC correlation token round-trip.
//!
//! Drive the full inbound → outbound → callback cycle:
//!
//! 1. User sends `/narrate alice` → narrate tool runs.
//! 2. Adapter renders the narrate Surface (Text + Narration +
//!    Button). The Button's callback_data is an HMAC-signed token
//!    under the adapter's `correlation_key`.
//! 3. We intercept the `sendMessage` body the courier emitted,
//!    extract the token from `reply_markup.inline_keyboard`.
//! 4. We POST a synthetic `callback_query` update back at the
//!    webhook with that token in `data`.
//! 5. Triton verifies the HMAC, re-dispatches narrate(alice), and
//!    POSTs the result back to Telegram a second time.
//!
//! Plus negative cases: forged tokens get rejected at the inbound
//! boundary with a `phase: rejected` audit line.

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
const CORRELATION_KEY: &str = "32byte-correlation-key-for-test!";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
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
                ("correlation_key", CORRELATION_KEY),
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

fn callback_query(token: &str) -> Value {
    json!({
        "update_id": 200,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
            "message": {
                "message_id": 1,
                "from": { "id": 0, "is_bot": true, "first_name": "Bot" },
                "chat": { "id": 42, "type": "private" },
                "date": 1_700_000_000,
            },
            "data": token,
            "chat_instance": "abc"
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrate_button_round_trips_through_callback_query() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // Step 1: trigger /narrate alice. Expect the courier to POST a
    // sendMessage with reply_markup carrying a Refresh button.
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/narrate alice"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    let first = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        v.first().cloned()
    });
    let inline_keyboard = first.body["reply_markup"]["inline_keyboard"]
        .as_array()
        .expect("inline_keyboard present");
    assert_eq!(inline_keyboard.len(), 1, "expected one button row");
    let token = inline_keyboard[0][0]["callback_data"]
        .as_str()
        .expect("callback_data is a string")
        .to_string();
    assert!(!token.is_empty());

    // Step 2: send a synthetic callback_query update carrying that
    // token. Triton must verify the HMAC, decode (narrate, {subject:
    // "alice"}), dispatch, and ship a SECOND sendMessage back.
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&callback_query(&token))
        .send()
        .await
        .expect("POST callback_query");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Capture grows to 2; second message is the callback-driven
    // post-back. Its text must contain "Hello, alice." (narrate's
    // signature output) — confirming the (tool, args) round-tripped
    // correctly.
    let two = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        if v.len() >= 2 { Some(v) } else { None }
    });
    let second_text = two[1].body["text"]
        .as_str()
        .expect("text is a string")
        .to_string();
    assert!(
        second_text.contains("Hello, alice."),
        "callback-driven re-dispatch should re-run narrate(alice); got: {second_text}",
    );

    // Both dispatches share the protocol tag. Look for two
    // phase=dispatch audit lines.
    let dispatches: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:telegram"
                && v["tool"] == "narrate"
        })
        .count();
    assert_eq!(
        dispatches, 2,
        "expected two dispatch audit lines (one inbound + one callback)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_callback_token_is_rejected_with_phase_rejected() {
    // Token signed under a DIFFERENT key. Adapter's HMAC verify
    // must reject; we log `phase: rejected, result: error:auth`
    // and never re-dispatch.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let forged_key = b"a-totally-different-32byte-key!!";
    let forged_token = triton_correlation::encode("narrate", &json!({ "s": "evil" }), forged_key)
        .expect("forged token fits in cap");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&callback_query(&forged_token))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401, "forged token must 401");

    // Sleep a tick for the audit line to flush.
    std::thread::sleep(Duration::from_millis(150));
    let rejected: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "rejected"
                && v["result"] == "error:auth"
                && v["protocol"] == "messenger:telegram"
        })
        .count();
    assert!(
        rejected >= 1,
        "expected at least one rejected audit line for the forged callback"
    );

    // Critically: no second sendMessage shipped.
    assert!(
        telegram.captured().is_empty(),
        "forged callback must not trigger a post-back; captured: {:?}",
        telegram.captured()
    );
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
