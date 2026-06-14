//! `env://VARNAME` secret resolution for manifest credentials.
//!
//! The substrate decommissioned Vault; secrets arrive as container env
//! (GCP Secret Manager → kamal → env). Triton's manifest credentials
//! must therefore resolve from the environment, not only from Vault or
//! dev-mode literals. These two NO-MOCK, spawned-binary tests prove it:
//!
//! 1. `env_refs_resolve_and_courier` (TRITON_ENV=local) — a whatsapp_cloud
//!    adapter whose every credential is an `env://` ref boots, verifies the
//!    inbound HMAC with the env-resolved app secret, and couriers the
//!    reply to the FakeWhatsApp API with the env-resolved access token.
//!    Proves the refs materialise through the real adapter end-to-end.
//! 2. `env_refs_boot_in_production` (TRITON_ENV=nonprod) — the same manifest
//!    boots where inline literals are REFUSED (M-SECRETS-1). Proves `env://`
//!    is a production-safe ref, the whole point on a Vault-less substrate.
//!
//! No mocks: real binary, real HTTP, real HMAC, real process env.

use std::collections::HashMap;
use std::path::PathBuf;
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
        .join("fixtures/manifest-whatsapp-env-secrets.yaml")
        .display()
        .to_string()
}

/// The `env://` vars the manifest refs point at — what kamal would inject
/// from GCP Secret Manager on the substrate.
fn secret_env() -> [(String, String); 6] {
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
async fn env_refs_resolve_and_courier() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
    ]);
    env.extend(secret_env());
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // Sign with the SECRET VALUE that lives behind env://TRITON_WA_APP_SECRET.
    // If the ref were treated as the literal string "env://TRITON_WA_APP_SECRET"
    // the HMAC would mismatch and nothing would courier — so a successful
    // courier proves the env ref was materialised into the real secret.
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
        "webhook status {}",
        resp.status()
    );

    // The outbound courier POSTs with the env-resolved access token.
    let deadline = Instant::now() + Duration::from_secs(3);
    let captured = loop {
        let v = whatsapp.captured();
        if !v.is_empty() {
            break v;
        }
        assert!(
            Instant::now() < deadline,
            "no outbound courier POST within 3s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(captured.len(), 1, "expected exactly one outbound POST");
    let sent = &captured[0];
    assert_eq!(
        sent.phone_number_id, PHONE_NUMBER_ID,
        "phone id from env ref"
    );
    assert_eq!(
        sent.authorization,
        format!("Bearer {ACCESS_TOKEN}"),
        "bearer must come from the env-resolved outbound.token credential",
    );
    assert_eq!(sent.body["to"], KNOWN_WA_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_refs_boot_in_production() {
    // TRITON_ENV=nonprod refuses inline literals (M-SECRETS-1). The same
    // env:// manifest must boot here — that's the production-safety the
    // Vault-less substrate needs. graph.facebook.com is the canonical
    // outbound base (egress allowlist); the rasterizer URL must be a
    // non-loopback hostname (whatsapp_cloud uses the rasterizer).
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        (
            "TRITON_WHATSAPP_API_BASE".to_string(),
            "https://graph.facebook.com".to_string(),
        ),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            "https://rasterizer.test.ts.net".to_string(),
        ),
    ]);
    env.extend(secret_env());

    // spawn_with_env waits for /healthz; if the manifest were rejected
    // (literal-in-prod → exit 2) the bind never happens and this panics.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert_eq!(resp.status(), 200, "triton booted with env:// credentials");
}
