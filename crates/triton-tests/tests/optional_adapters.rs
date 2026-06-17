//! `TRITON_OPTIONAL_ADAPTERS` — skip a baked chat adapter (with a warn)
//! when its declared `env://` credential secret is absent, instead of
//! aborting the whole boot.
//!
//! Motivation: `deploy/triton/adapter.yaml` is baked into both the
//! internal upstream-dispatcher image (WhatsApp only, for the outbound
//! courier) and the public chat-ingress image (WhatsApp + Telegram).
//! The internal image has no Telegram secret — and must NOT be given
//! one, or it could hijack the gateway's Telegram webhook. It needs to
//! skip `telegram` while still booting `whatsapp`.
//!
//! Two NO-MOCK, spawned-binary tests prove the contract end to end:
//!
//!  1. `optional_skips_adapter_with_missing_secret` — the two-adapter
//!     manifest boots with `TRITON_OPTIONAL_ADAPTERS=telegram` even
//!     though `env://TRITON_TELEGRAM_SECRET_TOKEN` is UNSET; the
//!     WhatsApp adapter still resolves its env secrets and couriers a
//!     real reply. Telegram is skipped, not fatal.
//!  2. `missing_secret_without_optin_aborts_boot` — the SAME manifest
//!     with the opt-in unset exits non-zero on the unset telegram
//!     secret, exactly as today (the default is unchanged; the gateway,
//!     which sets nothing, still fails loudly).
//!
//! No mocks: real binary, real HTTP, real HMAC, real process env.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const VERIFY_TOKEN: &str = "meta-verify-token-for-test";
const ACCESS_TOKEN: &str = "whatsapp-access-token-for-test";
const PHONE_NUMBER_ID: &str = "100200300";
const CORRELATION_KEY: &str = "whatsapp-correlation-key-for-test";
const KNOWN_WA_ID: &str = "491701234567";
const SENDER_TABLE: &str = r#"{"491701234567":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-optional-adapters.yaml")
        .display()
        .to_string()
}

/// The WhatsApp `env://` vars the manifest refs point at. Note we
/// deliberately do NOT inject `TRITON_TELEGRAM_SECRET_TOKEN`, so the
/// telegram adapter's `inbound.secret` is the unset case under test.
fn whatsapp_secret_env() -> [(String, String); 6] {
    [
        ("TRITON_WA_APP_SECRET".into(), APP_SECRET.into()),
        ("TRITON_WA_VERIFY_TOKEN".into(), VERIFY_TOKEN.into()),
        ("TRITON_WA_ACCESS_TOKEN".into(), ACCESS_TOKEN.into()),
        ("TRITON_WA_PHONE_NUMBER_ID".into(), PHONE_NUMBER_ID.into()),
        ("TRITON_WA_SENDER_TABLE".into(), SENDER_TABLE.into()),
        ("TRITON_WA_CORRELATION_KEY".into(), CORRELATION_KEY.into()),
    ]
}

fn whatsapp_inbound(wa_id: &str, text: &str) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{
            "id": "0",
            "changes": [{
                "value": {
                    "messaging_product": "whatsapp",
                    "metadata": {
                        "display_phone_number": "15555555555",
                        "phone_number_id": PHONE_NUMBER_ID,
                    },
                    "contacts": [{ "profile": { "name": "Alice" }, "wa_id": wa_id }],
                    "messages": [{
                        "from": wa_id,
                        "id": "wamid.HBgM",
                        "timestamp": "1700000000",
                        "type": "text",
                        "text": { "body": text }
                    }]
                },
                "field": "messages"
            }]
        }]
    })
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn optional_skips_adapter_with_missing_secret() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        // The opt-in: telegram's unset env:// secret is skippable.
        (
            "TRITON_OPTIONAL_ADAPTERS".to_string(),
            "telegram".to_string(),
        ),
    ]);
    env.extend(whatsapp_secret_env());
    // spawn_with_env waits for /healthz and panics if the child exits
    // early. So a successful spawn already proves the unset telegram
    // secret did NOT abort boot — telegram was skipped.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // The WhatsApp adapter must still be fully wired: sign with the
    // real env-resolved app secret and confirm an outbound courier POST.
    let envelope = whatsapp_inbound(KNOWN_WA_ID, "hello world");
    let body = serde_json::to_vec(&envelope).expect("json");
    let sig = sign(&body, APP_SECRET);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST webhook");
    assert!(
        resp.status().is_success(),
        "whatsapp webhook status {}",
        resp.status()
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let captured = loop {
        let v = whatsapp.captured();
        if !v.is_empty() {
            break v;
        }
        assert!(
            Instant::now() < deadline,
            "no outbound courier POST within 3s — whatsapp adapter did not survive the telegram skip"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(captured.len(), 1, "expected exactly one outbound POST");
    assert_eq!(
        captured[0].authorization,
        format!("Bearer {ACCESS_TOKEN}"),
        "bearer must come from the env-resolved whatsapp outbound token",
    );

    // The telegram skip is logged as a warning naming the adapter + var.
    let logs = proc.stderr_snapshot().join("\n") + "\n" + &proc.stdout_snapshot().join("\n");
    assert!(
        logs.contains("telegram") && logs.contains("TRITON_TELEGRAM_SECRET_TOKEN"),
        "expected a skip warning naming telegram + the missing var, got:\n{logs}"
    );
}

#[test]
fn missing_secret_without_optin_aborts_boot() {
    // Same manifest, telegram's env:// secret still unset, but NO opt-in.
    // This is the default the public gateway runs under: a missing chat
    // secret must fail boot loudly, never silently drop the adapter.
    let bin = locate_triton_binary();
    let mut cmd = Command::new(&bin);
    cmd.env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path());
    // WhatsApp secrets present (so the ONLY failure is telegram's unset
    // secret), but TRITON_OPTIONAL_ADAPTERS deliberately left unset.
    for (k, v) in whatsapp_secret_env() {
        cmd.env(k, v);
    }
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "without the opt-in, an unset telegram env secret must exit 2; got {:?}",
        out.status
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("TRITON_TELEGRAM_SECRET_TOKEN"),
        "expected the boot abort to name the unset var, got:\n{combined}"
    );
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let candidate_debug = here.join("target/debug/triton");
        let candidate_release = here.join("target/release/triton");
        if candidate_debug.exists() {
            return candidate_debug;
        }
        if candidate_release.exists() {
            return candidate_release;
        }
        here.pop();
    }
    panic!("could not locate `triton` binary");
}
