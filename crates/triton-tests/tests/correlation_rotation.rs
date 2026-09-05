//! #287 — the correlation key must be rotatable without invalidating
//! every button already sitting in a conversation.
//!
//! Before this, `correlation_key` was a single value used to both sign
//! and verify. Changing it broke every outstanding token at once: a
//! card minted a minute earlier stopped responding to a click. An
//! operator facing "rotate the key and break every live button" does
//! not rotate the key, so in practice the key was permanent — and a
//! secret nobody can rotate is a secret nobody can recover from.
//!
//! The fix is an overlap window. `correlation_key` accepts a
//! comma-separated LIST: tokens are signed with the FIRST key and
//! verified against ALL of them. Rotation becomes three safe steps:
//! prepend the new key, deploy, and drop the old one once every token
//! minted under it has expired.
//!
//! These drive the real Telegram webhook with a real token minted
//! under the OLD key — the operator's live buttons — and assert the
//! two ends of the window: it works mid-rotation, and it stops working
//! once the old key is dropped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;

const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";
/// The key the live buttons in this test were minted under.
const OLD_KEY: &str = "32byte-correlation-key-for-test!";
/// The key the operator is rotating TO.
const NEW_KEY: &str = "a-freshly-minted-correlation-key";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

/// `correlation_key_spec` is what the operator puts in the secret:
/// one key, or several separated by commas during a rotation.
fn env_with(telegram: &FakeTelegramApi, correlation_key_spec: &str) -> HashMap<String, String> {
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
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            correlation_key_spec.to_string(),
        ),
    ])
}

fn token_minted_under(key: &str) -> String {
    triton_correlation::encode_bound(
        "narrate",
        &json!({ "subject": "alice" }),
        key.as_bytes(),
        triton_correlation::PLATFORM_MAX_CALLBACK_DATA,
        triton_correlation::Binding {
            platform: "telegram",
            tenant: "acme",
            sender: "42",
        },
        None,
    )
    .expect("token fits")
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

/// Returns the webhook's status: a token the ring cannot verify is
/// refused at the inbound boundary with 401, so the status IS the
/// verdict here and each test asserts the one it expects.
async fn post_callback(webhook_addr: std::net::SocketAddr, token: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&callback_query(token))
        .send()
        .await
        .expect("POST callback")
        .status()
}

/// Mid-rotation: the operator has PREPENDED the new key and deployed.
/// Every button minted under the old key must keep working, or the
/// rotation is the outage it was designed to avoid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_button_minted_under_the_previous_key_survives_rotation() {
    let telegram = FakeTelegramApi::start().await;
    let spec = format!("{NEW_KEY},{OLD_KEY}");
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, &spec)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let status = post_callback(webhook_addr, &token_minted_under(OLD_KEY)).await;
    assert!(
        status.is_success(),
        "a token minted under a key still on the ring must be honoured; got {status}",
    );

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
    assert_eq!(dispatch["result"], "ok");

    // And the courier actually replied — the click did the work, not
    // merely passed a signature check.
    let reply = wait_for(Duration::from_secs(3), || {
        telegram.captured().first().cloned()
    });
    let text = reply.body["text"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("Hello, alice."),
        "the rotated-through click must re-run narrate(alice); got: {text}",
    );
}

/// A token signed with the NEW key must verify too — otherwise the
/// ring is only reading the tail of the list and every token minted
/// after the deploy is dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_new_key_is_the_one_that_signs() {
    let telegram = FakeTelegramApi::start().await;
    let spec = format!("{NEW_KEY},{OLD_KEY}");
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, &spec)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let status = post_callback(webhook_addr, &token_minted_under(NEW_KEY)).await;
    assert!(
        status.is_success(),
        "the first key on the ring is the signing key; got {status}",
    );

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
    assert_eq!(dispatch["result"], "ok");
}

/// The far end of the window: once the old key is dropped from the
/// list, tokens minted under it are refused. Without this the overlap
/// would be permanent and the rotation would never actually revoke
/// anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_old_key_ends_the_overlap_window() {
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, NEW_KEY)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let status = post_callback(webhook_addr, &token_minted_under(OLD_KEY)).await;
    assert_eq!(
        status, 401,
        "a token minted under a key no longer on the ring must be refused",
    );

    let rejected = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected"
    });
    assert_eq!(
        rejected["protocol"], "messenger:telegram",
        "the refusal must come from the inbound boundary; got: {rejected}",
    );
    assert!(
        !telegram.captured().iter().any(|c| c.body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Hello")),
        "a token off the ring must not dispatch",
    );
}

/// Whitespace around a key is an operator typo waiting to happen — a
/// secret pasted as `new, old` must not silently produce a key with a
/// leading space that verifies nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_are_trimmed_so_a_pasted_list_still_works() {
    let telegram = FakeTelegramApi::start().await;
    let spec = format!("{NEW_KEY} ,  {OLD_KEY}  ");
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, &spec)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let status = post_callback(webhook_addr, &token_minted_under(OLD_KEY)).await;
    assert!(
        status.is_success(),
        "surrounding whitespace must not change a key; got {status}",
    );

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
    assert_eq!(dispatch["result"], "ok");
}

/// A ring that parses to nothing must refuse the BOOT, not wait to be
/// discovered by the first click. `,` is a plausible operator slip when
/// editing a list, and an empty ring has no key to sign with — the
/// alternative to failing here is an adapter that mints tokens nobody
/// can verify and refuses every button, silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_key_list_refuses_to_boot() {
    let bin = {
        let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let cand = here.join("target/debug/triton");
            if cand.exists() {
                break cand;
            }
            assert!(here.pop(), "triton binary not found");
        }
    };
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path())
        .env("TRITON_TELEGRAM_API_BASE", "http://127.0.0.1:1")
        .env("TRITON_TG_WEBHOOK_SECRET", RESOLVED_SECRET)
        .env("TRITON_TG_BOT_TOKEN", BOT_TOKEN)
        .env(
            "TRITON_TG_SENDERS",
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#,
        )
        // Every entry is empty: a list with nothing on it.
        .env("TRITON_TG_CORRELATION_KEY", " , ,")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "an empty correlation-key ring must fail boot;\n{combined}"
    );
    assert!(
        combined.contains("correlation_key"),
        "the refusal must name the field the operator has to fix; got:\n{combined}"
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
