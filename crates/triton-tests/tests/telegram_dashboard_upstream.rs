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
use std::process::{Command, Stdio};
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

/// #143 review ([HIGH]): the delegated renderer must re-apply the
/// sidecar's `MAX_RESPONSE_BYTES` cap. An oversized `png_base64` is
/// rejected, so the dashboard falls back to text (no `sendPhoto`) rather
/// than allocating an unbounded PNG.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dashboard_delegation_rejects_oversize_png() {
    let telegram = FakeTelegramApi::start().await;
    // ~3.1 MiB of base64 → decodes to > 2 MiB (the MAX_RESPONSE_BYTES cap).
    let oversize = "A".repeat(3 * 1024 * 1024 + 256 * 1024);
    let renderer = FakeAgent::start_returning(json!({ "png_base64": oversize })).await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
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

    // Fallback text lands; no oversized PNG is ever shipped.
    let captured = wait_for(Duration::from_secs(5), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected one sendMessage fallback");
    assert!(
        telegram.captured_photos().is_empty(),
        "oversize PNG must not be shipped: {:?}",
        telegram.captured_photos()
    );
}

/// #143 review ([MEDIUM]): `TRITON_RASTERIZE_UPSTREAM` naming a tool that
/// isn't registered must fail boot (exit 2), not fail silently at the
/// first dashboard render.
#[test]
fn delegation_to_unregistered_tool_aborts_boot() {
    let bin = locate_triton_binary();
    let out = Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path())
        .env("TRITON_TELEGRAM_API_BASE", "http://127.0.0.1:1")
        .env("TRITON_TG_WEBHOOK_SECRET", RESOLVED_SECRET)
        .env("TRITON_TG_BOT_TOKEN", BOT_TOKEN)
        .env(
            "TRITON_TG_SENDERS",
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#,
        )
        .env("TRITON_TG_CORRELATION_KEY", CORRELATION_KEY)
        // Opt in to delegation, but DON'T register the tool.
        .env("TRITON_RASTERIZE_UPSTREAM", "render_a2ui_to_png")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "delegation to an unregistered tool must exit 2; got {:?}",
        out.status
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("render_a2ui_to_png"),
        "boot abort should name the unregistered tool, got:\n{combined}"
    );
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        for cand in ["target/debug/triton", "target/release/triton"] {
            let p = here.join(cand);
            if p.exists() {
                return p;
            }
        }
        here.pop();
    }
    panic!("could not locate `triton` binary");
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
