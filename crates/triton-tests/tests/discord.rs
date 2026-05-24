//! v0.2 PR 22 — Discord adapter integration tests.
//!
//! Drives the full inbound flow against the real binary:
//!   * Ed25519 signature verification (valid + tampered)
//!   * PING/PONG handshake
//!   * button-click round-trip (custom_id = HMAC correlation token
//!     under the same shared `triton-correlation` crate that PR 21
//!     ships for Telegram)
//!
//! No mocks: real binary, real HTTP, real Ed25519 keypair, real
//! Vault KV v2 fake serving the public key + sender_table.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";
const CORRELATION_KEY: &str = "32byte-correlation-key-discord!!";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-discord-test.yaml")
        .display()
        .to_string()
}

/// Generate a fresh Ed25519 keypair and pre-bake all Vault values
/// (sender table, correlation key, public key hex) the manifest
/// references. Returns the keypair so the test can sign requests
/// after the binary boots.
async fn start_kv_vault_with_keypair() -> (FakeVault, SigningKey) {
    let signing = SigningKey::generate(&mut OsRng);
    let verifying = signing.verifying_key();
    let pk_hex = hex::encode(verifying.as_bytes());

    let vault = FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/discord",
            &[
                ("public_key", pk_hex.as_str()),
                ("bot_token", "stub-bot-token"),
                (
                    "senders",
                    r#"{"99":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#,
                ),
                ("correlation_key", CORRELATION_KEY),
            ],
        )],
    )
    .await;
    (vault, signing)
}

fn env_with(vault: &FakeVault) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
    ])
}

/// Current RFC 3339 UTC timestamp — what Discord stamps on the
/// message that carries the button. PR 23's freshness gate
/// requires this on every type=3 interaction.
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
        .unwrap()
        .to_rfc3339()
}

/// Sign a request body with the given Ed25519 keypair, return the
/// (timestamp, signature_hex) pair Discord's webhook would carry.
fn sign(signing: &SigningKey, body: &[u8]) -> (String, String) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let mut message = Vec::with_capacity(ts.len() + body.len());
    message.extend_from_slice(ts.as_bytes());
    message.extend_from_slice(body);
    let sig = signing.sign(&message);
    (ts, hex::encode(sig.to_bytes()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_returns_pong_under_valid_signature() {
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let body = json!({ "type": 1 }).to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST PING");
    assert!(resp.status().is_success(), "{}", resp.status());
    let pong: Value = resp.json().await.expect("PONG json");
    assert_eq!(pong["type"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_signature_is_rejected_with_phase_rejected() {
    let (vault, _signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Sign with a DIFFERENT keypair — adapter's public_key won't
    // verify it.
    let attacker = SigningKey::generate(&mut OsRng);
    let body = json!({ "type": 1 }).to_string();
    let (ts, sig) = sign(&attacker, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST forged");
    assert_eq!(resp.status(), 401, "forged signature MUST 401");

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:discord"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn button_click_dispatches_via_correlation_token() {
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Mint a correlation token for narrate(subject=bob) under the
    // same key the manifest declares — that's how Discord buttons
    // come back to us after the user clicks.
    let token = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "bob" }),
        CORRELATION_KEY.as_bytes(),
    )
    .expect("token fits");

    let interaction = json!({
        "type": 3, // MESSAGE_COMPONENT
        "id": "i-1",
        "application_id": "app-1",
        "token": "interaction-token",
        "user": { "id": "99" },
        "data": { "custom_id": token, "component_type": 2 },
        "message": { "timestamp": now_rfc3339() }
    });
    let body = interaction.to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST button");
    assert!(resp.status().is_success(), "{}", resp.status());

    let body: Value = resp.json().await.expect("response body");
    assert_eq!(body["type"], 4, "expected CHANNEL_MESSAGE_WITH_SOURCE");
    let content = body["data"]["content"].as_str().expect("content string");
    assert!(
        content.contains("Hello, bob") && content.contains("narration about bob"),
        "expected narrate output in response content; got: {content}"
    );
    // Narrate's Button component should round-trip into Discord's
    // components v2: ActionRow → Button with a fresh correlation
    // token in custom_id.
    let components = body["data"]["components"]
        .as_array()
        .expect("components array");
    assert_eq!(components[0]["type"], 1);
    let buttons = components[0]["components"].as_array().unwrap();
    let nested_token = buttons[0]["custom_id"]
        .as_str()
        .expect("custom_id is a string");
    let (tool, _) =
        triton_correlation::decode(nested_token, CORRELATION_KEY.as_bytes()).expect("verifies");
    assert_eq!(tool, "narrate");

    // Audit: one dispatch + one post for this interaction.
    let dispatches: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:discord"
                && v["tool"] == "narrate"
        })
        .count();
    assert!(dispatches >= 1, "expected a dispatch audit line");
    let posts: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:discord"
        })
        .count();
    assert!(posts >= 1, "expected a post audit line");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_custom_id_token_rejected_at_inbound() {
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let forged = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "evil" }),
        b"wrong-correlation-key-32-bytes!!",
    )
    .expect("forged token fits");
    let interaction = json!({
        "type": 3,
        "id": "i-2",
        "user": { "id": "99" },
        "data": { "custom_id": forged },
        "message": { "timestamp": now_rfc3339() }
    });
    let body = interaction.to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST forged token");
    assert_eq!(resp.status(), 401);
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:discord"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_message_timestamp_rejects_button_click() {
    // PR 23 replay protection. We sign a VALID Ed25519 envelope
    // with a VALID correlation token, but attach an
    // `interaction.message.timestamp` that's 1 hour in the past.
    // Adapter must refuse before reaching the dispatcher.
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let valid_token = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "bob" }),
        CORRELATION_KEY.as_bytes(),
    )
    .expect("token fits");

    let stale_iso = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // 1 hour ago, RFC 3339 UTC.
        let stale = now - 3600;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(stale, 0).unwrap();
        dt.to_rfc3339()
    };

    let interaction = json!({
        "type": 3,
        "id": "i-stale",
        "user": { "id": "99" },
        "data": { "custom_id": valid_token, "component_type": 2 },
        "message": { "timestamp": stale_iso }
    });
    let body = interaction.to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST stale");
    assert_eq!(resp.status(), 401, "stale message timestamp MUST 401");
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:discord"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_succeeds_then_excess_is_ratelimited() {
    // PR 24: NFR-P-3 per-adapter rate limit. Mirror of the
    // Telegram rate_limit.rs integration test for the Discord
    // path, since signature/route shape diverges (Codex PR 24
    // concern — single-adapter coverage hides regressions).
    let (vault, signing) = {
        // Reuse the start_kv_vault_with_keypair vault layout but
        // point the manifest at a fixture with a tiny rate limit
        // (rate=1/sec, burst=2). The vault payload itself is
        // identical to manifest-discord-test.yaml.
        let signing = SigningKey::generate(&mut OsRng);
        let pk_hex = hex::encode(signing.verifying_key().as_bytes());
        let vault = FakeVault::start_kv_v2(
            VAULT_TOKEN,
            &[(
                "kv/data/triton-test/discord",
                &[
                    ("public_key", pk_hex.as_str()),
                    ("bot_token", "stub-bot-token"),
                    (
                        "senders",
                        r#"{"99":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#,
                    ),
                    ("correlation_key", CORRELATION_KEY),
                ],
            )],
        )
        .await;
        (vault, signing)
    };

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-discord-rate-limit.yaml")
        .display()
        .to_string();
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Each Discord interaction is independently signed → bake a
    // closure that signs+POSTs and returns the status code +
    // Retry-After (if any). Burst=2: first two pass, next two
    // 429 + Retry-After.
    let valid_token = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "bob" }),
        CORRELATION_KEY.as_bytes(),
    )
    .expect("token fits");
    let send = |i: u32| {
        let signing = &signing;
        let interaction = json!({
            "type": 3,
            "id": format!("i-{i}"),
            "user": { "id": "99" },
            "data": { "custom_id": valid_token, "component_type": 2 },
            "message": { "timestamp": now_rfc3339() }
        });
        let body = interaction.to_string();
        let (ts, sig) = sign(signing, body.as_bytes());
        let url = format!("http://{webhook}/discord/interactions");
        async move {
            let resp = reqwest::Client::new()
                .post(&url)
                .header("X-Signature-Ed25519", sig)
                .header("X-Signature-Timestamp", ts)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("POST");
            (
                resp.status().as_u16(),
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string()),
            )
        }
    };

    assert_eq!(send(1).await.0, 200);
    assert_eq!(send(2).await.0, 200);
    let (s3, retry3) = send(3).await;
    assert_eq!(s3, 429);
    assert!(retry3.is_some(), "expected Retry-After header on 429");
    let (s4, _) = send(4).await;
    assert_eq!(s4, 429);

    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:ratelimit"
            && v["protocol"] == "messenger:discord"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_callback_substitutes_picked_value_and_dispatches() {
    // PR 25: a string-select callback carries the chosen value in
    // `data.values[0]`. The mapper emitted the menu with a token
    // encoding `(tool, {args_key: null})`; the inbound handler
    // substitutes the null with the chosen value, then dispatches.
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Mint a token shaped the way the mapper would: tool=narrate,
    // args = {subject: null}. The handler MUST substitute the
    // null with `data.values[0]` before invoking the tool.
    let select_token = triton_correlation::encode(
        "narrate",
        &json!({ "subject": serde_json::Value::Null }),
        CORRELATION_KEY.as_bytes(),
    )
    .expect("token fits");

    let interaction = json!({
        "type": 3, // MESSAGE_COMPONENT
        "id": "i-select",
        "user": { "id": "99" },
        "data": {
            "custom_id": select_token,
            "component_type": 3, // STRING_SELECT
            "values": ["bob"]
        },
        "message": { "timestamp": now_rfc3339() }
    });
    let body = interaction.to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST select");
    assert!(resp.status().is_success(), "{}", resp.status());

    let body: Value = resp.json().await.expect("response body");
    assert_eq!(body["type"], 4);
    let content = body["data"]["content"].as_str().expect("content");
    // narrate was invoked with subject="bob" (substituted from
    // data.values[0]). Its surface text contains "Hello, bob".
    assert!(
        content.contains("Hello, bob"),
        "selection callback should dispatch narrate(subject=bob); got: {content}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_message_timestamp_fails_closed() {
    // Codex PR 23 review blocker: absence of the freshness anchor
    // MUST NOT bypass replay protection. A type=3 interaction
    // without `message` (or with an empty `message.timestamp`)
    // gets 401 + a rejected audit, NOT through to dispatch.
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let valid_token = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "bob" }),
        CORRELATION_KEY.as_bytes(),
    )
    .expect("token fits");
    // Note: no `message` field at all.
    let interaction = json!({
        "type": 3,
        "id": "i-no-ts",
        "user": { "id": "99" },
        "data": { "custom_id": valid_token }
    });
    let body = interaction.to_string();
    let (ts, sig) = sign(&signing, body.as_bytes());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/discord/interactions"))
        .header("X-Signature-Ed25519", sig)
        .header("X-Signature-Timestamp", ts)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST missing-timestamp");
    assert_eq!(resp.status(), 401, "missing timestamp MUST 401");
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:discord"
    });
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
