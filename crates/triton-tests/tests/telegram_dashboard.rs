//! v0.2 PR 36 — end-to-end dashboard rasterisation through the
//! Telegram adapter.
//!
//! Drives a tool that emits a Dashboard surface (`demo_panel`),
//! ensures the adapter:
//!   * calls the live `triton-rasterizer` sidecar,
//!   * receives the PNG,
//!   * dispatches `sendPhoto` (NOT sendMessage) with the PNG +
//!     caption + reply_markup,
//!   * emits the rasterizer-call audit line at `phase: post`.
//!
//! When the rasterizer is configured but down, the adapter MUST
//! fall back to `sendMessage` with a placeholder so the user
//! still gets SOMETHING.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::rasterizer_fixture::RasterizerProcess;
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
                ("correlation_key", CORRELATION_KEY),
            ],
        )],
    )
    .await
}

fn env_with(
    vault: &FakeVault,
    telegram: &FakeTelegramApi,
    rasterizer_url: &str,
) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            rasterizer_url.to_string(),
        ),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dashboard_surface_dispatches_sendphoto_with_png() {
    // `/demo` runs DemoPanel, whose Surface includes a Dashboard
    // component. With the rasterizer up, the adapter MUST call
    // sendPhoto (not sendMessage), ship the PNG bytes, and emit a
    // `phase: post, result: rasterizer_call` audit line.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    let raster = RasterizerProcess::spawn().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&vault, &telegram, &raster.url()),
    )
    .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/demo"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());

    // sendPhoto MUST land — exactly once.
    let photos = wait_for(Duration::from_secs(5), || {
        let v = telegram.captured_photos();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(photos.len(), 1, "expected exactly one sendPhoto");
    let photo = &photos[0];
    assert_eq!(
        photo.token, BOT_TOKEN,
        "courier MUST use the resolved bot token"
    );
    assert_eq!(photo.chat_id, "42");
    // Real PNG bytes from the rasterizer service.
    assert_eq!(
        &photo.photo_bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "expected PNG magic bytes in the photo file part"
    );
    assert!(
        photo.photo_bytes.len() > 200,
        "PNG body unexpectedly short ({} bytes)",
        photo.photo_bytes.len()
    );
    // Caption SHOULD include the surrounding Text / Narration
    // components from demo_panel's Surface (they precede the
    // Dashboard component).
    let caption = photo.caption.as_deref().unwrap_or("");
    assert!(
        caption.contains("Triton demo panel"),
        "caption should carry the leading Text component, got: {caption}"
    );
    // Inline keyboard from demo_panel's Refresh button rides on
    // the photo too (Telegram reply_markup on sendPhoto).
    assert!(
        photo
            .reply_markup
            .as_deref()
            .map(|s| s.contains("Refresh"))
            .unwrap_or(false),
        "expected the Refresh button to ship on sendPhoto reply_markup, got: {:?}",
        photo.reply_markup
    );
    // Tile content MUST NOT leak into the caption (that would
    // duplicate the rendered image content and silently violate
    // the `rasterised_png` degrade rule).
    assert!(!caption.contains("invocations"));
    assert!(!caption.contains("1,284"));

    // No sendMessage should have happened — the rasterizer was up
    // and the surface had a dashboard.
    assert!(
        telegram.captured().is_empty(),
        "expected zero sendMessage calls, got: {:?}",
        telegram.captured()
    );

    // Audit: a `rasterizer_call` line at `phase: post` MUST be
    // emitted alongside the actual post.
    std::thread::sleep(Duration::from_millis(150));
    let audits: Vec<serde_json::Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let rasterizer_audits: Vec<&serde_json::Value> = audits
        .iter()
        .filter(|v| v["kind"] == "audit" && v["status_detail"] == "rasterizer_call")
        .collect();
    assert!(
        !rasterizer_audits.is_empty(),
        "expected a `rasterizer_call` audit line; got audits: {audits:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rasterizer_failure_falls_back_to_text() {
    // Configure the adapter to a dead rasterizer port. The
    // dashboard surface still completes — the adapter falls back
    // to sendMessage with a deferred-text placeholder so the user
    // sees something. Audit MUST log the rasterizer failure.
    let vault = start_kv_vault().await;
    let telegram = FakeTelegramApi::start().await;
    // Reserve a port by binding then dropping; nothing is listening
    // on it for the duration of the test.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    };
    let dead_url = format!("http://127.0.0.1:{dead_port}");

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&vault, &telegram, &dead_url),
    )
    .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/demo"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());

    // sendMessage MUST be the fallback shape (no sendPhoto).
    let captured = wait_for(Duration::from_secs(5), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert!(
        telegram.captured_photos().is_empty(),
        "expected zero sendPhoto on rasterizer-failure; got: {:?}",
        telegram.captured_photos(),
    );
    assert_eq!(
        captured.len(),
        1,
        "expected exactly one sendMessage fallback"
    );
    let body = &captured[0].body;
    let text = body["text"].as_str().unwrap_or("");
    assert!(
        text.contains("dashboard") && text.contains("unavailable"),
        "expected fallback placeholder mentioning unavailable dashboard; got: {text}"
    );
    // No tile content in the fallback text either.
    assert!(!text.contains("invocations"));
    assert!(!text.contains("1,284"));

    // Audit: rasterizer_failed status_label MUST appear.
    std::thread::sleep(Duration::from_millis(150));
    let audits: Vec<serde_json::Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let fails: Vec<&serde_json::Value> = audits
        .iter()
        .filter(|v| v["kind"] == "audit" && v["status_detail"] == "rasterizer_failed")
        .collect();
    assert!(
        !fails.is_empty(),
        "expected a `rasterizer_failed` audit line; got audits: {audits:#?}"
    );
    let fail = fails[0];
    assert_eq!(fail["result"], "error:provider");
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
