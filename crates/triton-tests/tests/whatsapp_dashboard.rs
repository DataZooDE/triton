//! v0.2 PR 38 — end-to-end dashboard rasterisation through the
//! WhatsApp adapter.
//!
//! WhatsApp Cloud API supports image messages via a two-step flow:
//!   1. POST the PNG bytes to `/v18.0/{phone_number_id}/media` →
//!      get back a stable `media_id`.
//!   2. POST `{type: "image", image: {id: "<media_id>"}}` to
//!      `/v18.0/{phone_number_id}/messages`.
//!
//! Two scenarios:
//!
//! 1. `whatsapp_dashboard_uploads_media_then_sends_image` — `/demo`
//!    via inbound webhook drives `demo_panel`, whose Surface
//!    contains a `Component::Dashboard`. Adapter MUST upload the
//!    rendered PNG to `/media` and then POST an image-typed
//!    `/messages`. Audit emits the `rasterizer_call` line.
//!
//! 2. `whatsapp_dashboard_falls_back_to_text_on_rasterizer_failure`
//!    — rasterizer down; only ONE `/messages` POST happens, with a
//!    plain text body mentioning "unavailable". Audit shows
//!    `rasterizer_failed`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::rasterizer_fixture::RasterizerProcess;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const ACCESS_TOKEN: &str = "whatsapp-access-token-for-test";
const PHONE_NUMBER_ID: &str = "100200300";
const KNOWN_WA_ID: &str = "491701234567";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-dashboard.yaml")
        .display()
        .to_string()
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
                    "contacts": [{
                        "profile": { "name": "Alice" },
                        "wa_id": wa_id,
                    }],
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
    let out = mac.finalize().into_bytes();
    format!("sha256={}", hex::encode(out))
}

fn env_with(whatsapp: &FakeWhatsAppApi, rasterizer_url: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            rasterizer_url.to_string(),
        ),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whatsapp_dashboard_uploads_media_then_sends_image() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let raster = RasterizerProcess::spawn().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp, &raster.url()))
            .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let envelope = whatsapp_inbound(KNOWN_WA_ID, "/demo");
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
    assert!(resp.status().is_success(), "{}", resp.status());

    // Media upload captured exactly once — the PNG body contains
    // PNG magic bytes from the real rasterizer.
    let media = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured_media();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(media.len(), 1, "expected exactly one media upload");
    let upload = &media[0];
    assert_eq!(upload.phone_number_id, PHONE_NUMBER_ID);
    assert_eq!(
        upload.authorization,
        format!("Bearer {ACCESS_TOKEN}"),
        "media upload bearer MUST come from the resolved access_token",
    );
    assert_eq!(upload.messaging_product, "whatsapp");
    assert_eq!(upload.kind, "image/png");
    assert_eq!(
        &upload.photo_bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "expected PNG magic in media upload body",
    );
    assert!(
        upload.photo_bytes.len() > 200,
        "PNG body unexpectedly short ({} bytes)",
        upload.photo_bytes.len(),
    );

    // Message captured exactly once — type=image carrying the
    // stub media_id the fixture returned.
    let messages = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(messages.len(), 1, "expected exactly one /messages POST");
    let sent = &messages[0];
    assert_eq!(sent.phone_number_id, PHONE_NUMBER_ID);
    assert_eq!(sent.body["type"], "image");
    assert_eq!(sent.body["image"]["id"], "media_id_stub");
    // Tile content MUST NOT leak into any caption.
    let caption = sent.body["image"]
        .get("caption")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert!(!caption.contains("invocations"));
    assert!(!caption.contains("1,284"));

    // Audit: `rasterizer_call` line emitted alongside the post.
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
                && v["status_detail"] == "rasterizer_call"
                && v["protocol"] == "messenger:whatsapp"
        })
        .collect();
    assert!(
        !rasterizer_audits.is_empty(),
        "expected a `rasterizer_call` audit line; got audits: {audits:#?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_dashboard_falls_back_to_text_on_rasterizer_failure() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    };
    let dead_url = format!("http://127.0.0.1:{dead_port}");

    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&whatsapp, &dead_url)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let envelope = whatsapp_inbound(KNOWN_WA_ID, "/demo");
    let body = serde_json::to_vec(&envelope).expect("json");
    let sig = sign(&body, APP_SECRET);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    let captured = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert!(
        whatsapp.captured_media().is_empty(),
        "expected zero media uploads on rasterizer-failure; got: {:?}",
        whatsapp.captured_media(),
    );
    assert_eq!(
        captured.len(),
        1,
        "expected exactly one /messages POST fallback",
    );
    let sent = &captured[0];
    assert_eq!(sent.body["type"], "text");
    let text = sent.body["text"]["body"].as_str().unwrap_or("");
    assert!(
        text.contains("dashboard") && text.contains("unavailable"),
        "expected fallback placeholder mentioning unavailable dashboard; got: {text}",
    );
    // Tile content MUST stay out of the fallback body.
    assert!(!text.contains("invocations"));
    assert!(!text.contains("1,284"));

    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["status_detail"] == "rasterizer_failed"
            && v["protocol"] == "messenger:whatsapp"
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
