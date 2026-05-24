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
        "data": { "custom_id": token, "component_type": 2 }
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
        "data": { "custom_id": forged }
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
