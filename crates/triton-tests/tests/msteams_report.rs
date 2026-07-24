//! Issue #200 — MS Teams inline Report chart images + card theming.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256 JWT
//! verification against `FakeBotFramework`, and real upstream agents
//! (`FakeAgent`) reached over TCP for `assistant` / `render_report` /
//! `get_theme`.
//!
//! - A message routes to the `assistant` upstream, which returns a
//!   surface carrying a `Report`. The adapter dispatches `render_report`
//!   (a second upstream returning an inline `png_base64`), caches the
//!   PNG, and embeds it in the reply as an Adaptive Card `Image` served
//!   from a signed `…/img/{token}` route.
//! - `get_theme` brands the reply card header (title + banner logo).
//! - A forged / wrong-marker image token 404s at the `…/img/` route.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";
const CORRELATION_KEY: &[u8] = b"correlation-key-for-test";
const PUBLIC_BASE: &str = "https://teams.example";

/// The tiniest valid PNG (1×1), base64 — as an upstream returns it inline.
const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAP8/xJ8oAAAAAElFTkSuQmCC";

fn report_manifest() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-report-test.yaml")
        .display()
        .to_string()
}

fn base_env(fake: &FakeBotFramework, upstreams: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), report_manifest()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        (
            "TRITON_MSTEAMS_PUBLIC_BASE".to_string(),
            PUBLIC_BASE.to_string(),
        ),
        ("TRITON_STATIC_UPSTREAMS".to_string(), upstreams.to_string()),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn good_claims(fake: &FakeBotFramework) -> Value {
    json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceUrl": fake.service_url(),
    })
}

fn message_activity(text: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": text,
        "textFormat": "plain"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_surface_renders_chart_image_via_signed_route() {
    let fake = FakeBotFramework::start().await;
    // `assistant` returns a surface with a Report; `render_report` returns
    // a peacock-shaped artifact carrying the chart PNG in `_meta`.
    let assistant = FakeAgent::start_returning(json!({
        "surface": { "components": [
            { "kind": "text", "value": "Alpine dominates your spend." },
            { "kind": "report", "report_id": "supplier-concentration", "args": { "top_n": 10 } }
        ] }
    }))
    .await;
    let report = FakeAgent::start_returning(json!({
        "isError": false,
        "content": [{ "type": "text", "text": "Supplier concentration" }],
        "structuredContent": { "result": { "_meta": { "png_base64": TINY_PNG_B64 } } }
    }))
    .await;
    let upstreams = format!(
        "assistant={},render_report={}",
        assistant.host_port(),
        report.host_port()
    );
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), base_env(&fake, &upstreams)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("show me the concentration chart"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The reply Activity carries an Adaptive Card whose Image points at
    // the signed /msteams/img route.
    let reply = wait_for(Duration::from_secs(3), || {
        fake.captured().into_iter().next()
    });
    let content = &reply.body["attachments"][0]["content"];
    assert_eq!(
        content["type"], "AdaptiveCard",
        "reply must be an AdaptiveCard; got: {}",
        reply.body
    );
    let img_url = content["body"]
        .as_array()
        .and_then(|b| b.iter().find(|w| w["type"] == "Image"))
        .and_then(|img| img["url"].as_str())
        .unwrap_or_else(|| panic!("expected an Image widget; got: {content}"));
    assert!(
        img_url.starts_with(PUBLIC_BASE) && img_url.contains("/msteams/img/"),
        "image points at the signed /img route: {img_url}"
    );
    // The base64 PNG must NOT be dumped into the card text.
    assert!(
        !content.to_string().contains("png_base64"),
        "must not leak the base64 PNG into the card"
    );

    // The /img route serves the actual PNG bytes.
    let token = img_url.rsplit('/').next().expect("token in url");
    let img = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{token}"))
        .send()
        .await
        .expect("GET img");
    assert!(img.status().is_success(), "img route: {}", img.status());
    assert_eq!(
        img.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png"),
    );
    let bytes = img.bytes().await.expect("png bytes");
    assert_eq!(&bytes[..4], b"\x89PNG", "expected PNG magic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_theme_brands_the_reply_card() {
    let fake = FakeBotFramework::start().await;
    // `assistant` returns a narration + button surface (so the reply is a
    // card, where branding applies).
    let assistant = FakeAgent::start_returning(json!({
        "surface": { "components": [
            { "kind": "narration", "text": "top risks" },
            { "kind": "button", "label": "Details", "tool": "assistant", "args": {} }
        ] }
    }))
    .await;
    let theme = FakeAgent::start_returning(json!({
        "title": "DataZoo Supplier Risk",
        "logo_url": "https://brand.example/logo.png",
        "logo_style": "banner",
        "brand_color": "#1A73E8",
    }))
    .await;
    let upstreams = format!(
        "assistant={},get_theme={}",
        assistant.host_port(),
        theme.host_port()
    );
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), base_env(&fake, &upstreams)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("which suppliers are risky?"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    let reply = wait_for(Duration::from_secs(3), || {
        fake.captured().into_iter().next()
    });
    let body = reply.body["attachments"][0]["content"]["body"]
        .as_array()
        .expect("card body");
    // Banner logo is the first element.
    assert_eq!(
        body[0]["type"],
        "Image",
        "banner logo first; got: {reply:?}",
        reply = reply.body
    );
    assert_eq!(body[0]["url"], "https://brand.example/logo.png");
    // The title TextBlock is present.
    assert!(
        body.iter()
            .any(|w| w["type"] == "TextBlock" && w["text"] == "DataZoo Supplier Risk"),
        "themed title in card; got: {}",
        reply.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_image_token_returns_404() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(json!({ "echo": "x" })).await;
    let upstreams = format!("assistant={}", assistant.host_port());
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), base_env(&fake, &upstreams)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let client = reqwest::Client::new();

    // Token signed under a DIFFERENT key → bad signature.
    let wrong_key = triton_correlation::encode_with_cap(
        "__render_report_png",
        &json!({ "id": "whatever" }),
        b"a-totally-different-key!!",
        8192,
    )
    .expect("fits");
    let r1 = client
        .get(format!("http://{webhook}/msteams/img/{wrong_key}"))
        .send()
        .await
        .expect("GET");
    assert_eq!(r1.status(), 404, "wrong-key image token must 404");

    // Valid key, but a button-style marker replayed at /img → 404.
    let wrong_marker =
        triton_correlation::encode_with_cap("narrate", &json!({ "s": "x" }), CORRELATION_KEY, 8192)
            .expect("fits");
    let r2 = client
        .get(format!("http://{webhook}/msteams/img/{wrong_marker}"))
        .send()
        .await
        .expect("GET");
    assert_eq!(r2.status(), 404, "wrong-marker token must 404");

    // Garbage token → 404, not a panic.
    let r3 = client
        .get(format!("http://{webhook}/msteams/img/not-a-token"))
        .send()
        .await
        .expect("GET");
    assert_eq!(r3.status(), 404);
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
