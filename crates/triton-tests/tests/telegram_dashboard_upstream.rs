//! Issue #143 (D) — delegate chat dashboard rasterisation to a
//! registered upstream tool `render_a2ui_to_png` instead of the in-tree
//! `triton-rasterizer` sidecar.
//!
//! Same `/demo` → Dashboard-surface flow as `telegram_dashboard.rs`, but
//! with `TRITON_RASTERIZE_UPSTREAM=render_a2ui_to_png` set and NO sidecar
//! running. The adapter MUST call the upstream tool with the dashboard
//! spec and ship the PNG bytes the upstream returned (proven by an
//! upstream-only marker the sidecar would never produce).
//!
//! The sidecar path (delegation disabled) stays covered by
//! `telegram_dashboard.rs::dashboard_surface_dispatches_sendphoto_with_png`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::upstream_fixture::FakeAgent;

const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";
const CORRELATION_KEY: &str = "correlation-key-from-vault";

/// PNG bytes only the upstream renderer would emit — valid PNG magic plus
/// a marker the in-tree sidecar (which renders a real dashboard) never
/// produces. Lets the test prove the bytes came from the delegation path.
fn upstream_png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(b"PEACOCK-UPSTREAM-RENDER");
    v
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dashboard_delegates_rasterisation_to_upstream_render_a2ui_to_png() {
    let telegram = FakeTelegramApi::start().await;
    // The upstream `render_a2ui_to_png` returns a base64 PNG. No sidecar
    // is started; delegation is the only rasterisation path.
    let png_b64 = base64::engine::general_purpose::STANDARD.encode(upstream_png());
    let renderer = FakeAgent::start_returning(json!({ "png_base64": png_b64 })).await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        // Opt in to upstream delegation, and register the tool.
        (
            "TRITON_RASTERIZE_UPSTREAM".to_string(),
            "render_a2ui_to_png".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("render_a2ui_to_png={}", renderer.host_port()),
        ),
        (
            "TRITON_TG_WEBHOOK_SECRET".to_string(),
            RESOLVED_SECRET.to_string(),
        ),
        ("TRITON_TG_BOT_TOKEN".to_string(), BOT_TOKEN.to_string()),
        (
            "TRITON_TG_SENDERS".to_string(),
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            CORRELATION_KEY.to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update("/demo"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());

    // sendPhoto MUST land, carrying the UPSTREAM's PNG bytes verbatim.
    let photos = wait_for(Duration::from_secs(5), || {
        let v = telegram.captured_photos();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(photos.len(), 1, "expected exactly one sendPhoto");
    assert_eq!(
        photos[0].photo_bytes,
        upstream_png(),
        "sendPhoto did not carry the upstream renderer's bytes"
    );

    // The upstream renderer was called with the dashboard spec
    // (`{title, tiles}`) under a minted bearer.
    let bodies = renderer.bodies_seen();
    assert_eq!(
        bodies.len(),
        1,
        "expected one render_a2ui_to_png call: {bodies:?}"
    );
    assert!(
        bodies[0]["title"].is_string() && bodies[0]["tiles"].is_array(),
        "render_a2ui_to_png args should be the dashboard spec, got: {}",
        bodies[0]
    );
    assert_eq!(
        renderer.tools_seen(),
        vec![Some("render_a2ui_to_png".to_string())]
    );
    assert!(!renderer.bearers_seen()[0].is_empty(), "no bearer minted");

    // No sendMessage fallback — delegation succeeded.
    assert!(
        telegram.captured().is_empty(),
        "expected zero sendMessage calls, got: {:?}",
        telegram.captured()
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
