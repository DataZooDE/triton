//! #352 step 0: the platform payload each courier-backed channel produces
//! today for one fixed rich surface, pinned byte for byte.
//!
//! The surface hint (#352) has to leave channel output untouched. An agent
//! that ignores the hint must get exactly what it gets today. Comparing
//! hint-on against hint-off after the change cannot prove that, because both
//! modes would share any regression the change introduces. So these fixtures
//! were captured from the code as it was *before* any #352 production change,
//! and every later step must keep them byte-identical.
//!
//! Volatile values (correlation tokens, ids, timestamps) are redacted by key
//! before comparing; everything the user sees is kept.
//!
//! Regenerate deliberately, and only for an intended rendering change, with:
//! `UPDATE_SURFACE_BASELINE=1 cargo test -p triton-tests --test it surface_baseline`
//!
//! No mocks: real spawned binary, real signed webhooks, a real upstream
//! (`FakeAgent`) and the in-repo platform fakes over real TCP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use hmac::{Hmac, Mac};
use rand_core::OsRng;
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::{
    FakeBotFramework, FakeGoogleJwks, FakeTelegramApi, FakeWhatsAppApi,
};
use triton_tests::signald_fixture::FakeSignald;
use triton_tests::upstream_fixture::FakeAgent;

/// The one rich answer every channel renders: a markdown table the text-first
/// channels can't show, a narration line, four follow-up buttons (more than
/// WhatsApp's three reply buttons), and sources. The buttons' tool and args are
/// deliberately tiny: a signed correlation token must fit the 64-byte callback
/// cap (Telegram, WhatsApp), or the button is deferred and the baseline would
/// never exercise the buttons path.
fn rich_surface() -> Value {
    json!({ "surface": { "components": [
        { "kind": "text", "value": "**Q3 revenue** by region:\n\n| Region | Revenue | Growth |\n|---|---|---|\n| EMEA | 1.2M | +4% |\n| APAC | 0.8M | +9% |\n| AMER | 2.1M | -1% |" },
        { "kind": "narration", "text": "APAC is growing fastest." },
        { "kind": "button", "label": "Drill into EMEA", "tool": "go", "args": { "r": "EU" } },
        { "kind": "button", "label": "Drill into APAC", "tool": "go", "args": { "r": "AP" } },
        { "kind": "button", "label": "Drill into AMER", "tool": "go", "args": { "r": "AM" } },
        { "kind": "button", "label": "Compare to Q2", "tool": "go", "args": { "r": "Q2" } },
        { "kind": "sources", "items": [
            { "label": "Revenue report", "resource": "ui://peacock/report/revenue?q=3" }
        ] }
    ] } })
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
        .display()
        .to_string()
}

fn baseline_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/surface-baseline")
}

/// Keys whose values change on every run: signed correlation tokens embed an
/// expiry. Their *presence* stays pinned; only the value is masked. A Google
/// Chat action parameter `{"key":"ct","value":…}` and a WhatsApp reply button's
/// `reply.id` carry the same token under a generic key, so they are matched by
/// context.
const VOLATILE_KEYS: &[&str] = &["custom_id", "ct", "callback_data"];

fn redact(v: &mut Value) {
    redact_in(v, "");
}

fn redact_in(v: &mut Value, parent: &str) {
    match v {
        Value::Object(map) => {
            let is_ct_param = map.get("key").and_then(Value::as_str) == Some("ct");
            for (k, val) in map.iter_mut() {
                let volatile = VOLATILE_KEYS.contains(&k.as_str())
                    || (is_ct_param && k == "value")
                    || (parent == "reply" && k == "id");
                if volatile && val.is_string() {
                    *val = json!("<redacted>");
                } else {
                    redact_in(val, k);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| redact_in(i, parent)),
        _ => {}
    }
}

/// Compare against the checked-in baseline, or write it when explicitly asked.
fn pin(name: &str, mut produced: Value) {
    redact(&mut produced);
    let path = baseline_dir().join(name);
    let text = format!("{}\n", serde_json::to_string_pretty(&produced).unwrap());
    if std::env::var("UPDATE_SURFACE_BASELINE").is_ok() {
        std::fs::create_dir_all(baseline_dir()).unwrap();
        std::fs::write(&path, &text).expect("write baseline");
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — generate with UPDATE_SURFACE_BASELINE=1",
            path.display()
        )
    });
    assert_eq!(
        on_disk,
        text,
        "{} changed: the platform payload for the baseline surface is no longer \
         what the channel produced before #352. The surface hint must not change \
         channel output.",
        path.display()
    );
}

/// One driven turn: the upstream that answered it, the platform payload the
/// channel produced, and the process kept alive until the caller is done.
pub struct Turn {
    pub agent: FakeAgent,
    pub payload: Value,
    _proc: TritonProcess,
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("condition not met within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub async fn drive_whatsapp_cloud(surface: Value) -> Turn {
    drive_whatsapp_cloud_with("manifest-whatsapp-cloud-inbound-tool.yaml", surface).await
}

/// [`drive_whatsapp_cloud`] against another fixture manifest.
pub async fn drive_whatsapp_cloud_with(manifest: &str, surface: Value) -> Turn {
    const APP_SECRET: &str = "whatsapp-app-secret-for-test";
    let agent = FakeAgent::start_returning(surface).await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), fixture(manifest)),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let body = serde_json::to_vec(&json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": "100200300" },
            "messages": [{ "from": "491701234567", "id": "wamid.X", "timestamp": "1700000000",
                "type": "text", "text": { "body": "revenue please" } }]
        }, "field": "messages" }] }]
    }))
    .unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(APP_SECRET.as_bytes()).unwrap();
    mac.update(&body);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Let the courier finish every send for this turn before pinning.
    let sent = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    std::thread::sleep(Duration::from_millis(500));
    let sent = {
        let all = whatsapp.captured();
        if all.len() > sent.len() { all } else { sent }
    };
    Turn {
        payload: Value::Array(sent.into_iter().map(|m| m.body).collect()),
        agent,
        _proc: proc,
    }
}

pub async fn drive_telegram(surface: Value) -> Turn {
    const SECRET: &str = "secret-resolved-from-vault";
    let agent = FakeAgent::start_returning(surface).await;
    let telegram = FakeTelegramApi::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-surface-baseline-telegram.yaml"),
        ),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
        ("TRITON_TG_WEBHOOK_SECRET".to_string(), SECRET.to_string()),
        (
            "TRITON_TG_BOT_TOKEN".to_string(),
            "12345:resolved-bot-token".to_string(),
        ),
        (
            "TRITON_TG_SENDERS".to_string(),
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            "correlation-key-from-vault".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .json(&json!({
            "update_id": 100,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
                "chat": { "id": 42, "type": "private" },
                "date": 1_700_000_000,
                "text": "revenue please"
            }
        }))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    wait_for(Duration::from_secs(5), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    std::thread::sleep(Duration::from_millis(500));
    Turn {
        payload: Value::Array(telegram.captured().into_iter().map(|m| m.body).collect()),
        agent,
        _proc: proc,
    }
}

pub async fn drive_signal(surface: Value) -> Turn {
    let agent = FakeAgent::start_returning(surface).await;
    let signald = FakeSignald::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-surface-baseline-signal.yaml"),
        ),
        ("TRITON_SIGNAL_SIGNALD_ADDR".to_string(), signald.tcp_uri()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("subscribe");

    signald.emit_incoming(
        "00000000-0000-0000-0000-000000000001",
        Some("+15559999999"),
        "revenue please",
    );
    let send = signald
        .wait_for_type("send", Duration::from_secs(5))
        .await
        .expect("send line");
    Turn {
        payload: send.value,
        agent,
        _proc: proc,
    }
}

pub async fn drive_discord(surface: Value) -> Turn {
    let agent = FakeAgent::start_returning(surface).await;
    let signing = SigningKey::generate(&mut OsRng);
    let pk_hex = hex::encode(signing.verifying_key().as_bytes());
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-discord-test.yaml"),
        ),
        ("TRITON_DISCORD_PUBLIC_KEY".to_string(), pk_hex),
        (
            "TRITON_DISCORD_BOT_TOKEN".to_string(),
            "stub-bot-token".to_string(),
        ),
        (
            "TRITON_DISCORD_SENDERS".to_string(),
            r#"{"99":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_DISCORD_CORRELATION_KEY".to_string(),
            "32byte-correlation-key-discord!!".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let body = json!({
        "type": 2,
        "id": "i-slash-1",
        "application_id": "app-1",
        "token": "interaction-token",
        "user": { "id": "99" },
        "data": { "name": "assistant",
                  "options": [ { "name": "message", "type": 3, "value": "revenue please" } ] }
    })
    .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let mut msg = ts.as_bytes().to_vec();
    msg.extend_from_slice(body.as_bytes());
    let sig = hex::encode(signing.sign(&msg).to_bytes());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST slash");
    assert!(resp.status().is_success(), "{}", resp.status());
    Turn {
        payload: resp.json().await.expect("interaction json"),
        agent,
        _proc: proc,
    }
}

pub async fn drive_google_chat(surface: Value) -> Turn {
    let agent = FakeAgent::start_returning(surface).await;
    let jwks = FakeGoogleJwks::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-google-chat-card-test.yaml"),
        ),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let jwt = jwks.sign_jwt(json!({
        "iss": "chat@system.gserviceaccount.com",
        "aud": "1234567890",
        "iat": now,
        "exp": now + 600,
        "sub": "google-chat-svc",
    }));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&json!({
            "type": "MESSAGE",
            "eventTime": "2026-05-25T10:00:00.000Z",
            "message": {
                "name": "spaces/AAA/messages/BBB",
                "sender": { "name": "users/99", "displayName": "Alice", "type": "HUMAN" },
                "text": "revenue please",
                "space": { "name": "spaces/AAA", "type": "DM" }
            },
            "space": { "name": "spaces/AAA", "type": "DM" },
            "user": { "name": "users/99" }
        }))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());
    Turn {
        payload: resp.json().await.expect("reply json"),
        agent,
        _proc: proc,
    }
}

pub async fn drive_msteams(surface: Value) -> Turn {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAgent::start_returning(surface).await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-msteams-async-test.yaml"),
        ),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        ("TRITON_MSTEAMS_ASYNC".to_string(), "1".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("answer={}", agent.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com",
        "aud": "triton-msteams-test-appid",
        "exp": now + 600,
        "iat": now - 5,
        "serviceurl": fake.service_url(),
    }));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&json!({
            "type": "message",
            "id": "msg-1",
            "timestamp": "2026-05-25T10:00:00.0000000Z",
            "serviceUrl": "https://placeholder.example/",
            "channelId": "msteams",
            "from": { "id": "29:1abc", "name": "Alice" },
            "conversation": { "id": "a:conv-1", "conversationType": "groupChat" },
            "recipient": { "id": "28:bot-1", "name": "MyBot" },
            "text": "revenue please",
            "textFormat": "plain"
        }))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Typing first, then the reply message: pin the reply.
    let reply = wait_for(Duration::from_secs(5), || {
        fake.captured()
            .into_iter()
            .map(|a| a.body)
            .find(|b| b["type"] == "message")
    });
    Turn {
        payload: reply,
        agent,
        _proc: proc,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_cloud_payload_is_pinned() {
    pin(
        "whatsapp_cloud.json",
        drive_whatsapp_cloud(rich_surface()).await.payload,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_payload_is_pinned() {
    pin(
        "telegram.json",
        drive_telegram(rich_surface()).await.payload,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_payload_is_pinned() {
    pin("signal.json", drive_signal(rich_surface()).await.payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discord_slash_command_payload_is_pinned() {
    pin("discord.json", drive_discord(rich_surface()).await.payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_chat_sync_reply_is_pinned() {
    pin(
        "google_chat.json",
        drive_google_chat(rich_surface()).await.payload,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn msteams_group_chat_payload_is_pinned() {
    pin("msteams.json", drive_msteams(rich_surface()).await.payload);
}
