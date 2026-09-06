//! #287 — a correlation token must be usable only by the sender it was
//! minted for.
//!
//! `tenant_key` derived from `(label, platform, tenant)`, so a token was
//! a capability held by the whole TENANT. Cross-tenant replay was
//! closed; intra-tenant replay was not. In a shared space — a Telegram
//! group, a Teams channel — `callback_data` is not a secret: any member
//! who can read another member's button can click it and have the tool
//! run under their OWN principal against the OTHER person's arguments.
//!
//! Folding the sender into the same derivation input closes it at zero
//! wire cost, exactly as the tenant binding did: the token minted for
//! alice fails bob's SIGNATURE rather than a comparison someone has to
//! remember to make.
//!
//! Both users below are in tenant `acme`, so the tenant binding cannot
//! be what refuses the replay.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;

const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";
const CORRELATION_KEY: &str = "32byte-correlation-key-for-test!";

const ALICE: u64 = 42;
const BOB: u64 = 43;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

/// Two senders, ONE tenant. The tenant binding is therefore satisfied
/// for both of them and cannot be what refuses anything here.
fn env_with(telegram: &FakeTelegramApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        (
            "TRITON_TG_WEBHOOK_SECRET".to_string(),
            RESOLVED_SECRET.to_string(),
        ),
        ("TRITON_TG_BOT_TOKEN".to_string(), BOT_TOKEN.to_string()),
        (
            "TRITON_TG_SENDERS".to_string(),
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                "43":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#
                .to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            CORRELATION_KEY.to_string(),
        ),
    ])
}

fn callback_query_from(user_id: u64, token: &str) -> Value {
    json!({
        "update_id": 200,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": user_id, "is_bot": false, "first_name": "User" },
            "message": {
                "message_id": 1,
                "from": { "id": 0, "is_bot": true, "first_name": "Bot" },
                // A GROUP chat: this is where a button is visible to
                // someone other than the person who triggered it.
                "chat": { "id": -100, "type": "group" },
                "date": now_secs(),
            },
            "data": token,
            "chat_instance": "abc"
        }
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The token alice's button carries, as the adapter mints it.
fn token_for(sender: &str) -> String {
    triton_correlation::encode_bound(
        "narrate",
        &json!({ "subject": "alice" }),
        CORRELATION_KEY.as_bytes(),
        triton_correlation::PLATFORM_MAX_CALLBACK_DATA,
        triton_correlation::Binding {
            platform: "telegram",
            tenant: "acme",
            sender,
        },
        None,
    )
    .expect("token fits")
}

async fn click(webhook_addr: std::net::SocketAddr, user_id: u64, token: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&callback_query_from(user_id, token))
        .send()
        .await
        .expect("POST callback")
        .status()
        .as_u16()
}

/// The replay this closes: bob clicks the button the bot rendered for
/// alice, in a group they share. Same tenant, so the #250 binding is
/// satisfied and cannot be what refuses him.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_members_button_is_not_another_members_capability() {
    let telegram = FakeTelegramApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let alices = token_for(&ALICE.to_string());
    let status = click(webhook_addr, BOB, &alices).await;

    assert_eq!(
        status, 401,
        "bob must not be able to click alice's button, even in the same tenant",
    );
    let rejected = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected"
    });
    assert_eq!(
        rejected["who"], "bob",
        "the refusal is audited against the CLICKER"
    );
    assert!(
        telegram.captured().is_empty(),
        "a refused click must produce no reply at all",
    );
}

/// The other half: alice's own button still works. A binding that
/// refuses everyone is not a binding, it is an outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sender_it_was_minted_for_can_still_click_it() {
    let telegram = FakeTelegramApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let status = click(webhook_addr, ALICE, &token_for(&ALICE.to_string())).await;
    assert!(status < 400, "alice's own button must work; got {status}");

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
    assert_eq!(dispatch["result"], "ok");
    assert_eq!(dispatch["who"], "alice");
}

/// End-to-end through the REAL mint: the button the adapter renders for
/// alice — not one the test hand-signed — is refused for bob. This is
/// what proves the adapter passes the right sender at mint time; a test
/// that only ever mints its own tokens would pass even if the adapter
/// bound every button to a constant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_button_the_adapter_actually_rendered_is_sender_bound() {
    let telegram = FakeTelegramApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // Alice triggers the command in the group; the bot renders a
    // Refresh button whose callback_data is the real minted token.
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&json!({
            "update_id": 100,
            "message": {
                "message_id": 1,
                "from": { "id": ALICE, "is_bot": false, "first_name": "Alice" },
                "chat": { "id": -100, "type": "group" },
                "date": now_secs(),
                "text": "/narrate alice"
            }
        }))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    let sent = wait_for(Duration::from_secs(3), || {
        telegram.captured().first().cloned()
    });
    let minted = sent.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("the rendered button carries a callback token")
        .to_string();

    assert_eq!(
        click(webhook_addr, BOB, &minted).await,
        401,
        "the token the adapter minted for alice must refuse bob",
    );
    assert!(
        click(webhook_addr, ALICE, &minted).await < 400,
        "…and must still work for alice",
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
