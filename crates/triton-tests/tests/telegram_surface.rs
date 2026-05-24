//! v0.2 PR 19 — L6' surface mapper for Telegram.
//!
//! When a tool returns an A2UI surface (`narrate` does), the
//! adapter must turn the components into a native Telegram
//! message: text and narration are passthrough (narration as
//! italics via HTML parse_mode), buttons defer to the HMAC
//! correlation-token PR.
//!
//! No mocks: real binary, real HTTP, real `FakeTelegramApi`.

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
async fn narrate_surface_is_rendered_as_html_italics() {
    // `narrate` returns Text + Narration + Button. PR 19 maps
    // text passthrough, narration to <i>...</i> in HTML mode, and
    // defers buttons (audited as "dropped" with a deferral reason
    // because correlation tokens land next PR).
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/narrate alice"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "webhook returned {}",
        resp.status()
    );

    let captured = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1);
    let sent = &captured[0];
    assert_eq!(sent.body["parse_mode"], "HTML");

    let text = sent.body["text"].as_str().expect("text is a string");
    // Text passthrough.
    assert!(
        text.contains("Hello, alice."),
        "expected passthrough Text component, got: {text}"
    );
    // Narration in italics.
    assert!(
        text.contains("<i>") && text.contains("</i>"),
        "expected <i>…</i> wrapping the narration, got: {text}"
    );
    assert!(
        text.contains("generated narration about alice"),
        "expected narration content, got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn html_special_chars_in_tool_output_are_escaped() {
    // The mapper uses HTML parse_mode for narration italics, so
    // any `<`, `>`, `&` in the tool's output MUST be HTML-escaped
    // or Telegram returns 400 "can't parse entities" — and worse,
    // a tool that returned `<script>` could otherwise inject HTML
    // through the post-back to a downstream renderer.
    //
    // We drive this with `/narrate <fragile&unsafe>` — `narrate`
    // embeds the subject in its text + narration verbatim. The
    // mapper must render `&lt;fragile&amp;unsafe&gt;` (escaped),
    // not the raw chars.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let _ = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/narrate <fragile&unsafe>"))
        .send()
        .await
        .expect("POST");

    let captured = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    let text = captured[0].body["text"]
        .as_str()
        .expect("text is a string")
        .to_string();
    assert!(
        text.contains("&lt;fragile&amp;unsafe&gt;"),
        "expected HTML-escaped subject, got: {text}"
    );
    assert!(
        !text.contains("<fragile"),
        "raw `<` must not appear, got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_response_stays_plain_text_no_parse_mode() {
    // Non-A2UI tool results (echo returns `{ "echo": "..." }`)
    // keep PR 18's bare-text path — no `parse_mode`, no HTML.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let _ = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("plain echo"))
        .send()
        .await
        .expect("POST");

    let captured = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured[0].body["text"], "plain echo");
    assert!(
        captured[0].body.get("parse_mode").is_none() || captured[0].body["parse_mode"].is_null(),
        "echo path must NOT set parse_mode; got: {}",
        captured[0].body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_surface_is_dropped_at_mapper_edge_no_courier_call() {
    // Codex PR 19 blocker 1: an empty Surface used to ship
    // `text: ""` to Telegram, which 400s. Per L6' spec the mapper
    // refuses at its edge. PR 20 audits this as
    // `phase: post, status_label: dropped` and skips the courier
    // call entirely (so the fake never sees a request).
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/empty"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "inbound must still ack 200: got {}",
        resp.status()
    );

    // No courier call should have happened.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        telegram.captured().is_empty(),
        "mapper must not call the Telegram API for an empty surface; captured: {:?}",
        telegram.captured()
    );

    // But the post-phase audit MUST land — operators need to see
    // every dispatch's outbound outcome.
    let mut audits: Vec<serde_json::Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    audits.retain(|v| v["kind"] == "audit" && v["phase"] == "post");
    assert!(
        !audits.is_empty(),
        "expected at least one phase:post audit line"
    );
    let post = &audits[0];
    assert_eq!(post["status_label"], "dropped");
    assert_eq!(post["result"], "error:provider");
    assert_eq!(post["tool"], "empty_surface");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrate_with_no_arg_routes_to_narrate_not_echo() {
    // Codex PR 19 concern: `/narrate` without a space silently
    // routed to echo (surprising). PR 20 fixes the parser so it
    // routes to narrate with an empty subject — narrate just
    // produces "Hello, ." which is harmless and visibly handled
    // instead of vanishing.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let _ = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/narrate"))
        .send()
        .await
        .expect("POST");

    let captured = wait_for(Duration::from_secs(2), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    let text = captured[0].body["text"]
        .as_str()
        .expect("text is a string")
        .to_string();
    assert!(
        text.contains("Hello, ."),
        "expected narrate's `Hello, .` (empty subject) shape, got: {text}",
    );
    // narrate emits a Narration component → HTML parse_mode set.
    assert_eq!(captured[0].body["parse_mode"], "HTML");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_components_are_logged_not_silently_dropped() {
    // `narrate` includes a Button component. PR 19 doesn't render
    // buttons (the HMAC correlation-token PR lands next), but it
    // MUST NOT silently drop them — a tracing::warn line names the
    // deferred component count so the operator can see how often
    // this happens in practice.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &telegram)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let _ = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/narrate alice"))
        .send()
        .await
        .expect("POST");
    // Sleep a tick to let the courier complete + the warn line land.
    std::thread::sleep(Duration::from_millis(200));

    let stdout = proc.stdout_snapshot().join("\n");
    assert!(
        stdout.contains("button") || stdout.contains("Button"),
        "expected tracing line naming deferred Button component; got stdout:\n{stdout}"
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
