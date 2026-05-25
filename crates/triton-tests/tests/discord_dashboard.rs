//! v0.2 PR 38 — end-to-end dashboard rasterisation through the
//! Discord adapter.
//!
//! Discord's interaction-response model is inline: the response
//! body IS the outbound. To attach an image to a type-4 channel
//! message, Discord requires the response itself to be
//! `multipart/form-data` carrying a `payload_json` part + one
//! `files[N]` part per attachment. PR 38 wires the rasterizer to
//! drive this path on a `Component::Dashboard` surface.
//!
//! Two scenarios:
//!
//! 1. `discord_dashboard_surface_responds_with_multipart_png` —
//!    slash command emits a Dashboard surface; with the rasterizer
//!    up the inline interaction response is multipart carrying real
//!    PNG bytes + a `payload_json` referencing `attachments[0]`.
//!    Audit shows the `rasterizer_call` line at `phase: post`.
//!
//! 2. `discord_dashboard_falls_back_to_text_on_rasterizer_failure`
//!    — same trigger against a dead rasterizer port; response is
//!    plain JSON (type 4) with a placeholder mentioning
//!    "unavailable". Audit shows `rasterizer_failed`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::rasterizer_fixture::RasterizerProcess;
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";
const CORRELATION_KEY: &str = "32byte-correlation-key-discord!!";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-discord-dashboard.yaml")
        .display()
        .to_string()
}

async fn start_kv_vault_with_keypair() -> (FakeVault, SigningKey) {
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
}

fn env_with(vault: &FakeVault, rasterizer_url: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            rasterizer_url.to_string(),
        ),
    ])
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discord_dashboard_surface_responds_with_multipart_png() {
    let (vault, signing) = start_kv_vault_with_keypair().await;
    let raster = RasterizerProcess::spawn().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &raster.url()))
            .await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let interaction = json!({
        "type": 2, // APPLICATION_COMMAND
        "id": "i-dash-1",
        "application_id": "app-1",
        "token": "interaction-token",
        "user": { "id": "99" },
        "data": { "name": "demo_panel", "options": [] }
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
        .expect("POST slash");
    assert!(resp.status().is_success(), "{}", resp.status());

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("multipart/form-data"),
        "expected multipart response Content-Type, got: {content_type}",
    );
    let body_bytes = resp.bytes().await.expect("body").to_vec();
    // The body MUST contain both the payload_json part and a
    // `files[0]` part carrying PNG magic bytes.
    let body_str = String::from_utf8_lossy(&body_bytes).into_owned();
    assert!(
        body_str.contains("name=\"payload_json\""),
        "expected payload_json multipart part in response",
    );
    assert!(
        body_str.contains("name=\"files[0]\""),
        "expected files[0] multipart part in response",
    );
    // PNG magic shows up inside the body — search rather than slice
    // because we don't know the exact offset (depends on boundary +
    // payload_json size).
    let magic = b"\x89PNG\r\n\x1a\n";
    let png_offset = body_bytes
        .windows(magic.len())
        .position(|w| w == magic)
        .expect("PNG magic bytes in multipart body");
    // PNG body sanity: real renders are well past 200 bytes.
    assert!(
        body_bytes.len() - png_offset > 200,
        "PNG content unexpectedly short",
    );
    // The payload_json part should reference an attachments[0] entry.
    assert!(
        body_str.contains("\"attachments\""),
        "expected payload_json to declare attachments",
    );

    // Audit: a `rasterizer_call` line at `phase: post` MUST be
    // emitted alongside the actual post.
    std::thread::sleep(Duration::from_millis(150));
    let audits: Vec<Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let rasterizer_audits: Vec<&Value> = audits
        .iter()
        .filter(|v| {
            v["kind"] == "audit"
                && v["status_label"] == "rasterizer_call"
                && v["protocol"] == "messenger:discord"
        })
        .collect();
    assert!(
        !rasterizer_audits.is_empty(),
        "expected a `rasterizer_call` audit line; got audits: {audits:#?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discord_dashboard_falls_back_to_text_on_rasterizer_failure() {
    let (vault, signing) = start_kv_vault_with_keypair().await;
    // Point the adapter at a dead port. Bind-then-drop reserves a
    // free port we know nothing is listening on.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    };
    let dead_url = format!("http://127.0.0.1:{dead_port}");
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&vault, &dead_url)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let interaction = json!({
        "type": 2,
        "id": "i-dash-fallback",
        "application_id": "app-1",
        "token": "interaction-token",
        "user": { "id": "99" },
        "data": { "name": "demo_panel", "options": [] }
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
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "fallback MUST be plain JSON, got Content-Type: {content_type}",
    );
    let resp_body: Value = resp.json().await.expect("json body");
    assert_eq!(resp_body["type"], 4, "expected CHANNEL_MESSAGE_WITH_SOURCE");
    let content = resp_body["data"]["content"].as_str().expect("content");
    assert!(
        content.contains("dashboard") && content.contains("unavailable"),
        "fallback must mention 'dashboard' + 'unavailable'; got: {content}",
    );
    // Raw tile content MUST NOT leak into the fallback (would
    // silently violate the `rasterised_png` degrade rule).
    assert!(!content.contains("1,284"));
    assert!(!content.contains("invocations"));

    // Audit: rasterizer_failed status_label MUST appear.
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["status_label"] == "rasterizer_failed"
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
