//! v0.2 PR 32 — Telegram numbered-prompts form rendering.
//!
//! When a tool returns an A2UI Surface containing exactly one
//! `Component::Form`, the Telegram adapter intercepts and runs the
//! per-chat state machine in `triton-chat-telegram::form_state`:
//!
//! 1. First inbound message that triggers the form (e.g.
//!    `/form_only_demo_multi`) → bot replies with the form's title
//!    followed by `1/N — <label> (required)`.
//! 2. Subsequent plain-text messages from the same (chat, sender)
//!    fill each field in order. The adapter coerces Integer /
//!    Boolean fields per their declared kind, re-prompts on parse
//!    error or required-empty.
//! 3. Once every field is filled, the adapter dispatches the
//!    form's `submit_tool` with the assembled args. The result
//!    ships through the normal surface mapper.
//! 4. `/cancel` mid-form clears the state.
//!
//! No mocks: real binary, real HTTP, real Telegram-shaped webhook
//! POSTs, real `FakeTelegramApi` capturing the sendMessage stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::{FakeTelegramApi, SentMessage};
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";
const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";
const CORRELATION_KEY: &str = "correlation-key-from-vault";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

/// Telegram `Update` for a private chat. `user_id` doubles as the
/// chat id for private 1-on-1 chats; the integration tests below
/// rely on that to drive multi-chat scenarios with distinct ids.
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
    // The PR 32 fixture has TWO senders so the per-chat-isolation
    // test can drive distinct (chat, sender) pairs; the other
    // tests use only `42` and ignore `99`.
    FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/telegram",
            &[
                ("webhook_secret", RESOLVED_SECRET),
                ("bot_token", BOT_TOKEN),
                (
                    "senders",
                    r#"{
                        "42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                        "99":{"sub":"bob","scopes":["chat"],"tenant":"acme"}
                    }"#,
                ),
                ("correlation_key", CORRELATION_KEY),
            ],
        )],
    )
    .await
}

/// PR 37 Finding 5 fixture: two Telegram user_ids both resolving to
/// the SAME `sub` (`alice`) but DIFFERENT tenants. Reproduces the
/// ambiguity the FormKey-by-sub code allowed: user 43 sending a
/// value used to advance user 42's form because the store scanned
/// for the first sub match.
async fn start_kv_vault_shared_sub() -> FakeVault {
    FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/telegram",
            &[
                ("webhook_secret", RESOLVED_SECRET),
                ("bot_token", BOT_TOKEN),
                (
                    "senders",
                    r#"{
                        "42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                        "43":{"sub":"alice","scopes":["chat"],"tenant":"beta"}
                    }"#,
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

/// Send one webhook POST with the right secret token. Returns the
/// HTTP status — every test that drives the happy path expects
/// 200, but we don't assert here so the negative tests can read
/// off a non-200 status without each one repeating the assertion.
async fn post_webhook(webhook_addr: std::net::SocketAddr, body: &Value) -> reqwest::StatusCode {
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(body)
        .send()
        .await
        .expect("POST webhook");
    resp.status()
}

/// Wait for the fake Telegram API to receive a message that the
/// `pred` accepts. Returns the matching `SentMessage`. Polls every
/// 20 ms; panics if no match arrives within `deadline`.
fn wait_for_send<F>(api: &FakeTelegramApi, deadline: Duration, mut pred: F) -> SentMessage
where
    F: FnMut(&SentMessage) -> bool,
{
    let start = Instant::now();
    loop {
        for sent in api.captured() {
            if pred(&sent) {
                return sent;
            }
        }
        if start.elapsed() > deadline {
            panic!(
                "no captured sendMessage matched within {deadline:?}; saw {} messages",
                api.captured().len()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_first_message_installs_state_and_prompts_field_one() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let status = post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi")).await;
    assert!(status.is_success(), "{status}");

    // The first sendMessage to chat_id 42 is the form's title +
    // the first numbered prompt for the `name` field.
    let sent = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 42
    });
    let text = sent.body["text"].as_str().expect("text str");
    assert!(
        text.contains("Quick feedback (multi)"),
        "expected form title in first prompt; got: {text}"
    );
    assert!(
        text.contains("1/3"),
        "expected 1-of-3 numbered prompt; got: {text}"
    );
    assert!(
        text.contains("name") && text.contains("required"),
        "expected `name (required)` in first prompt; got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_collects_all_fields_then_dispatches_submit_tool() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Install the form, then walk through three valid answers.
    assert!(
        post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    // Wait for the first prompt so the state machine is settled
    // before we feed the next message.
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("1/3"))
            .unwrap_or(false)
    });

    assert!(
        post_webhook(webhook, &telegram_update(42, "Alice"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("2/3"))
            .unwrap_or(false)
    });

    assert!(
        post_webhook(webhook, &telegram_update(42, "42"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("3/3"))
            .unwrap_or(false)
    });

    assert!(
        post_webhook(webhook, &telegram_update(42, "yes"))
            .await
            .is_success()
    );

    // The submit dispatch fires the `submitted_form` tool with the
    // assembled args; the post-back text contains the JSON of the
    // collected fields.
    let submitted = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("Alice") && s.contains("42") && s.contains("true"))
            .unwrap_or(false)
    });
    let text = submitted.body["text"].as_str().unwrap();
    assert!(text.contains("\"name\""));
    assert!(text.contains("\"age\""));
    assert!(text.contains("\"subscribe\""));

    // Dispatch audit MUST name the submit tool, not the form-
    // emitting tool.
    let _dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram"
            && v["tool"] == "submitted_form"
            && v["who"] == "alice"
            && v["tenant"] == "acme"
            && v["result"] == "ok"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_integer_field_rejects_non_numeric_then_advances() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    assert!(
        post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("1/3"))
            .unwrap_or(false)
    });
    // Field 1 (name): valid.
    assert!(
        post_webhook(webhook, &telegram_update(42, "Alice"))
            .await
            .is_success()
    );
    // After name, we're at 2/3 — `age`.
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("2/3"))
            .unwrap_or(false)
    });
    // Field 2 (age): garbage. Adapter re-prompts.
    assert!(
        post_webhook(webhook, &telegram_update(42, "not a number"))
            .await
            .is_success()
    );
    let reprompt = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("expected an integer") && s.contains("2/3"))
            .unwrap_or(false)
    });
    let txt = reprompt.body["text"].as_str().unwrap();
    assert!(
        txt.contains("not a number"),
        "expected the offending value in the re-prompt; got: {txt}"
    );
    // Now send a valid integer; the form advances to step 3.
    assert!(
        post_webhook(webhook, &telegram_update(42, "33"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("3/3"))
            .unwrap_or(false)
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_boolean_field_accepts_variants() {
    // Drive a fresh form for each variant; assert the submit
    // dispatched with the correct boolean coercion. We pick a
    // representative subset (the rest are covered by the unit
    // tests in `form_state.rs`).
    for (input, expected_substr) in [
        ("yes", "\"subscribe\":true"),
        ("Y", "\"subscribe\":true"),
        ("true", "\"subscribe\":true"),
        ("1", "\"subscribe\":true"),
        ("no", "\"subscribe\":false"),
        ("N", "\"subscribe\":false"),
        ("FALSE", "\"subscribe\":false"),
        ("0", "\"subscribe\":false"),
    ] {
        let vault = start_kv_vault().await;
        let telegram = FakeTelegramApi::start().await;
        let proc =
            TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram))
                .await;
        let webhook = proc.chat_webhook_addr.expect("listener bound");

        assert!(
            post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
                .await
                .is_success()
        );
        let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
            m.body["text"]
                .as_str()
                .map(|s| s.contains("1/3"))
                .unwrap_or(false)
        });
        post_webhook(webhook, &telegram_update(42, "Alice")).await;
        let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
            m.body["text"]
                .as_str()
                .map(|s| s.contains("2/3"))
                .unwrap_or(false)
        });
        post_webhook(webhook, &telegram_update(42, "33")).await;
        let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
            m.body["text"]
                .as_str()
                .map(|s| s.contains("3/3"))
                .unwrap_or(false)
        });
        post_webhook(webhook, &telegram_update(42, input)).await;

        let submitted = wait_for_send(&telegram, Duration::from_secs(2), |m| {
            m.body["text"]
                .as_str()
                .map(|s| s.contains(expected_substr))
                .unwrap_or(false)
        });
        assert!(
            submitted.body["text"]
                .as_str()
                .unwrap()
                .contains(expected_substr),
            "input `{input}` should have coerced to `{expected_substr}`"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_required_empty_reprompts_same_field() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    assert!(
        post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("1/3"))
            .unwrap_or(false)
    });
    // Empty body on required field → re-prompt at the SAME step.
    assert!(
        post_webhook(webhook, &telegram_update(42, ""))
            .await
            .is_success()
    );
    let _reprompt = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("required") && s.contains("1/3"))
            .unwrap_or(false)
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_cancel_clears_state_and_falls_through_to_route_command() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Install a form.
    assert!(
        post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("1/3"))
            .unwrap_or(false)
    });
    // /cancel clears the slot and replies "Form cancelled.".
    assert!(
        post_webhook(webhook, &telegram_update(42, "/cancel"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("Form cancelled"))
            .unwrap_or(false)
    });

    // A subsequent plain message routes via `route_command` as if
    // no form had ever been installed — i.e. it echoes back.
    assert!(
        post_webhook(webhook, &telegram_update(42, "hello after cancel"))
            .await
            .is_success()
    );
    let echo = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["text"]
            .as_str()
            .map(|s| s.contains("hello after cancel"))
            .unwrap_or(false)
    });
    // Confirm dispatch went to `echo`, not the form's submit tool.
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram"
            && v["tool"] == "echo"
            && v["who"] == "alice"
    });
    assert!(echo.body["text"].as_str().unwrap().contains("after cancel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_state_is_per_chat() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Two distinct senders / chats install forms simultaneously.
    // Sender 42 (alice) installs first, sender 99 (bob) installs
    // second. Each advances one field; their args must NOT cross.
    assert!(
        post_webhook(webhook, &telegram_update(42, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 42
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("1/3"))
                .unwrap_or(false)
    });
    assert!(
        post_webhook(webhook, &telegram_update(99, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 99
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("1/3"))
                .unwrap_or(false)
    });

    // Alice fills `name=Alice`, advances to 2/3.
    post_webhook(webhook, &telegram_update(42, "Alice")).await;
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 42
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("2/3"))
                .unwrap_or(false)
    });

    // Bob fills `name=Bob`, advances to 2/3.
    post_webhook(webhook, &telegram_update(99, "Bob")).await;
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 99
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("2/3"))
                .unwrap_or(false)
    });

    // Finish both forms with different values; assert that each
    // submission carries the right name.
    post_webhook(webhook, &telegram_update(42, "11")).await;
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 42
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("3/3"))
                .unwrap_or(false)
    });
    post_webhook(webhook, &telegram_update(42, "yes")).await;
    let alice_submission = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 42
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("submitted") || s.contains("Alice"))
                .unwrap_or(false)
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("Alice"))
                .unwrap_or(false)
    });
    let alice_text = alice_submission.body["text"].as_str().unwrap();
    assert!(alice_text.contains("Alice"), "got: {alice_text}");
    assert!(
        !alice_text.contains("Bob"),
        "leak alice ⇆ bob: {alice_text}"
    );

    post_webhook(webhook, &telegram_update(99, "22")).await;
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 99
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("3/3"))
                .unwrap_or(false)
    });
    post_webhook(webhook, &telegram_update(99, "no")).await;
    let bob_submission = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 99
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("Bob"))
                .unwrap_or(false)
    });
    let bob_text = bob_submission.body["text"].as_str().unwrap();
    assert!(bob_text.contains("Bob"));
    assert!(!bob_text.contains("Alice"), "leak bob ⇆ alice: {bob_text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_per_tenant_cap_evicts_oldest() {
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let mut env = env_with(&vault, &telegram);
    // Tiny cap so we can reach the eviction branch by installing
    // 3 forms instead of 100.
    env.insert(
        "TRITON_TELEGRAM_FORM_CAP_PER_TENANT".to_string(),
        "2".to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Three distinct (chat_id) installations under the same
    // tenant (`acme`) — sender 42 maps to alice/acme. We drive
    // three different chat_ids by spoofing both `chat.id` and
    // `from.id` on each update. Wait, sender_table is keyed by
    // `from.id` — so we need three updates with the same from.id
    // but different chat.ids. Easy: synthesise the update by hand.
    let make_update = |chat_id: i64, text: &str| -> Value {
        json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
                "chat": { "id": chat_id, "type": "private" },
                "date": 1_700_000_000,
                "text": text
            }
        })
    };

    // Install form in chat 1, then chat 2 — both within cap.
    assert!(
        post_webhook(webhook, &make_update(101, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 101
    });
    assert!(
        post_webhook(webhook, &make_update(102, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 102
    });

    // Third install hits the cap → chat 101's form is evicted.
    // The adapter audits the eviction as `phase: rejected`,
    // `result: error:validation` (the eviction is a tool-shape /
    // policy violation, not an auth failure).
    assert!(
        post_webhook(webhook, &make_update(103, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 103
    });

    let _eviction = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:telegram"
            && v["result"] == "error:validation"
            && v["tenant"] == "acme"
    });

    // Sanity: chat 101's form is gone — feeding a value there
    // routes through `route_command` and echoes back instead of
    // advancing a form.
    assert!(
        post_webhook(webhook, &make_update(101, "Alice"))
            .await
            .is_success()
    );
    let echo = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 101
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("Alice"))
                .unwrap_or(false)
    });
    assert!(echo.body["text"].as_str().unwrap().contains("Alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_state_isolates_two_senders_sharing_sub() {
    // PR 37 Finding 5: the FormKey USED to be `(chat_id, sender_sub)`
    // and the Complete path scanned `sender_table` for the FIRST
    // entry whose `sub` matched. When two telegram_user_ids map to
    // the same `sub` (different tenants), sender 43's message could
    // re-derive principal under sender 42's claims.
    //
    // After the fix the FormKey is `(chat_id, telegram_user_id)`,
    // so each user has an independent form slot. User 43 sending a
    // value in the same chat MUST NOT advance user 42's form.
    let vault = start_kv_vault_shared_sub().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Both senders share `chat_id = 1000` (a group chat). User 42
    // installs a form; user 43 sends a plain text. After the fix,
    // user 43's message routes through `route_command` (echo),
    // user 42's form stays at step 1/3.
    let make_update = |user_id: u64, chat_id: i64, text: &str| -> Value {
        json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "from": { "id": user_id, "is_bot": false, "first_name": "X" },
                "chat": { "id": chat_id, "type": "group" },
                "date": 1_700_000_000,
                "text": text
            }
        })
    };

    // User 42 (alice@acme) installs the form in chat 1000.
    assert!(
        post_webhook(webhook, &make_update(42, 1000, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _prompt = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 1000
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("1/3"))
                .unwrap_or(false)
    });

    // User 42's dispatch must have resolved to tenant acme.
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram"
            && v["tenant"] == "acme"
    });

    // User 43 (alice@BETA — same sub, different tenant) sends a
    // value in the same chat. The pre-fix bug: this would advance
    // user 42's form because the store keyed on `sub`. The fix:
    // user 43 has no active form, so route_command kicks in and
    // echoes back. The echo dispatch audit must be tagged
    // `tenant: beta`, NOT `tenant: acme`.
    assert!(
        post_webhook(webhook, &make_update(43, 1000, "hello"))
            .await
            .is_success()
    );

    let beta_dispatch = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram"
            && v["tenant"] == "beta"
    });
    assert_eq!(beta_dispatch["tool"], "echo");

    // Sanity: user 42's form is still active at step 1/3 — user 43's
    // message did NOT leak in as a form value. We verify by feeding
    // user 42 their actual next value and checking we move to 2/3.
    assert!(
        post_webhook(webhook, &make_update(42, 1000, "AliceName"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 1000
            && m.body["text"]
                .as_str()
                .map(|s| s.contains("2/3"))
                .unwrap_or(false)
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_eviction_audit_records_evictee_principal() {
    // PR 37 Finding 6: when a per-tenant LRU eviction fires, the
    // adapter MUST audit the dropped form's principal (sub/tenant),
    // NOT the installer's. The previous code wrote the installer's
    // sub into the audit line, masking whose state was actually
    // lost — useless to operators tracing why a tenant's form
    // disappeared.
    //
    // We drive 3 installs from 3 distinct (telegram_user_id) senders
    // under the same tenant. Cap = 2, so the third install evicts
    // the first. The eviction audit MUST name the FIRST installer's
    // sub, not the third's.
    //
    // The shared_sub fixture maps `42 -> alice@acme`,
    // `43 -> alice@beta`. We need a tenant-shared but sub-distinct
    // setup so we can prove the audit picks the right principal,
    // independent of which sender installed last. Use a small
    // 3-sender vault for this.
    let vault = FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/telegram",
            &[
                ("webhook_secret", RESOLVED_SECRET),
                ("bot_token", BOT_TOKEN),
                (
                    "senders",
                    r#"{
                        "42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                        "43":{"sub":"bob","scopes":["chat"],"tenant":"acme"},
                        "44":{"sub":"carol","scopes":["chat"],"tenant":"acme"}
                    }"#,
                ),
                ("correlation_key", CORRELATION_KEY),
            ],
        )],
    )
    .await;
    let telegram = FakeTelegramApi::start().await;
    let mut env = env_with(&vault, &telegram);
    env.insert(
        "TRITON_TELEGRAM_FORM_CAP_PER_TENANT".to_string(),
        "2".to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let make_update = |user_id: u64, chat_id: i64, text: &str| -> Value {
        json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "from": { "id": user_id, "is_bot": false, "first_name": "X" },
                "chat": { "id": chat_id, "type": "private" },
                "date": 1_700_000_000,
                "text": text
            }
        })
    };

    // alice (42) installs in chat 100. bob (43) installs in chat
    // 101. carol (44) installs in chat 102 — this evicts alice's
    // form (oldest).
    assert!(
        post_webhook(webhook, &make_update(42, 100, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 100
    });
    assert!(
        post_webhook(webhook, &make_update(43, 101, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 101
    });
    assert!(
        post_webhook(webhook, &make_update(44, 102, "/form_only_demo_multi"))
            .await
            .is_success()
    );
    let _ = wait_for_send(&telegram, Duration::from_secs(2), |m| {
        m.body["chat_id"] == 102
    });

    // Eviction audit must name `alice` (the evictee), NOT `carol`
    // (the installer). Same tenant — both are `acme`.
    let eviction = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:telegram"
            && v["result"] == "error:validation"
            && v["tenant"] == "acme"
    });
    assert_eq!(
        eviction["who"], "alice",
        "eviction audit MUST name the EVICTEE (alice), not the installer (carol). Full line: {eviction}"
    );
}
