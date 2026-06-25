//! Regression for the `needs_rasterizer` gate (#135 follow-up): a
//! Telegram adapter on the **long_poll** inbound must be able to ship a
//! rasterised dashboard, exactly like the webhook variant. Telegram's
//! `sendPhoto` is outbound Bot-API and identical on both inbound
//! transports — the gate previously only wired the rasterizer for
//! `inbound.kind == webhook`, so a long-poll deploy silently degraded
//! every dashboard to a text placeholder.
//!
//! No mocks per CLAUDE.md §1: real `triton`, real `triton-rasterizer`
//! sidecar, real long-poll worker driving `getUpdates` → dispatch →
//! `sendPhoto` against the `FakeTelegramApi`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::rasterizer_fixture::RasterizerProcess;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-longpoll-test.yaml")
        .display()
        .to_string()
}

fn update(update_id: i64, user_id: u64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "Alice" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            start.elapsed() <= deadline,
            "probe timed out after {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_poll_telegram_ships_a_rasterised_dashboard() {
    // `/demo` runs the DemoPanel tool, whose Surface includes a Dashboard
    // component → the adapter must rasterise it and `sendPhoto`.
    let telegram = FakeTelegramApi::with_updates(vec![update(1, 42, "/demo")]).await;
    let raster = RasterizerProcess::spawn().await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        ("TRITON_RASTERIZER_URL".to_string(), raster.url()),
        // Poll promptly in the test instead of the production hold.
        (
            "TRITON_TELEGRAM_LONGPOLL_TIMEOUT_SECS".to_string(),
            "0".to_string(),
        ),
        (
            "TRITON_TELEGRAM_LONGPOLL_BACKOFF_MS".to_string(),
            "50".to_string(),
        ),
    ]);
    let _proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // The long-poll worker must dispatch `/demo` and ship the dashboard
    // as a sendPhoto carrying real PNG bytes — NOT a text fallback.
    let photos = wait_for(Duration::from_secs(8), || {
        let v = telegram.captured_photos();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(photos.len(), 1, "expected exactly one sendPhoto");
    let photo = &photos[0];
    assert_eq!(photo.chat_id, "42");
    assert_eq!(
        &photo.photo_bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "long-poll dashboard must ship real PNG bytes",
    );
    assert!(
        photo.photo_bytes.len() > 200,
        "PNG body unexpectedly short ({} bytes)",
        photo.photo_bytes.len()
    );

    // The dashboard went as a photo, so there must be no text fallback
    // (that would mean the rasterizer was never wired — the bug).
    assert!(
        telegram.captured().is_empty(),
        "long-poll dashboard must not fall back to sendMessage; got: {:?}",
        telegram.captured()
    );
}
