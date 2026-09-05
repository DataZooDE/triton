//! Issue #200 — the msteams signed chart-image route, end to end.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256 Bot
//! Framework JWT verification against the in-repo `FakeBotFramework`, a
//! real `assistant` upstream answering with an A2UI `report` component,
//! and a real `render_report` upstream (peacock's shape) reached over
//! TCP through the static router.
//!
//! The route is STATELESS by design (#635 live): the token carries the
//! `render_report` args plus an expiry and the PNG is rendered on fetch,
//! so any replica can serve any link. Teams fetches the URL
//! anonymously — no Activity, no JWT — so the HMAC-signed token is the
//! only gate, which makes its three outcomes the contract worth
//! pinning: valid → the PNG, forged → 401, expired → 410.
//!
//! The unit tests cover the card SHAPE; this covers the wire.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";
/// The correlation key the fixture manifest pins.
const CORRELATION_KEY: &[u8] = b"correlation-key-for-test";
/// Marker `tool` slot of a chart-image token — namespaced away from
/// card-action tokens under the same key. Mirrors the adapter's private
/// `RENDER_REPORT_IMG_MARKER`; asserting on the literal pins the wire
/// constant, so a rename that would strand live 7-day links is caught.
const IMG_MARKER: &str = "__msteams_report_img";
/// Matches the adapter's `IMG_TOKEN_CAP`.
const IMG_TOKEN_CAP: usize = 1024;

/// The tiniest valid PNG (1×1), base64 — as peacock returns it inline.
const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAP8/xJ8oAAAAAElFTkSuQmCC";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-report-test.yaml")
        .display()
        .to_string()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn good_claims(fake: &FakeBotFramework) -> Value {
    json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
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

/// Boot Triton with the two upstreams the inline-Report path needs: an
/// `assistant` answering with a surface that carries a `report`
/// component, and a peacock-shaped `render_report` returning a PNG.
async fn spawn_with_report_upstreams(
    fake: &FakeBotFramework,
    assistant: &FakeAgent,
    report: &FakeAgent,
) -> TritonProcess {
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "assistant={},render_report={}",
                assistant.host_port(),
                report.host_port()
            ),
        ),
        // A reachable public base, else the adapter mints no image URL
        // and degrades to text. The HOST is deliberately not the test
        // listener: Teams needs a name IT can reach, and the courier
        // owns no request headers to derive one from — so the token,
        // not the host, is what the route round-trips on.
        (
            "TRITON_MSTEAMS_PUBLIC_BASE".to_string(),
            "https://teams.example".to_string(),
        ),
    ]);
    TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
}

/// An agent answering with an inline `Report`: "embed this rendered
/// report in your reply", no button click.
fn report_surface() -> Value {
    json!({
        "surface": { "components": [
            { "kind": "text", "value": "Supplier concentration is up." },
            { "kind": "report", "report_id": "supplier-concentration",
              "args": { "params": { "quarter": "Q3" } } }
        ] }
    })
}

/// What peacock's `render_report` returns: the chart as an inline base64
/// PNG under `structuredContent.result._meta.png_base64`.
fn peacock_png_result() -> Value {
    json!({
        "isError": false,
        "content": [{ "type": "text", "text": "Supplier concentration" }],
        "structuredContent": { "result": { "_meta": { "png_base64": TINY_PNG_B64 } } }
    })
}

/// The full inline-Report round trip: a surface carrying a `Report`
/// renders as an Adaptive Card `Image` pointing at the signed route, and
/// fetching that URL anonymously dispatches `render_report` and returns
/// the actual PNG bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_component_renders_a_chart_image_served_by_the_img_route() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(report_surface()).await;
    let report = FakeAgent::start_returning(peacock_png_result()).await;
    let proc = spawn_with_report_upstreams(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("how concentrated are our suppliers?"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The reply card carries an Image pointing at the signed route.
    let first = wait_for(Duration::from_secs(5), || {
        fake.captured().into_iter().next()
    });
    let card = &first.body["attachments"][0]["content"];
    assert_eq!(
        card["type"], "AdaptiveCard",
        "a Report must render as a card, not bare text; got: {}",
        first.body
    );
    let img = card["body"]
        .as_array()
        .expect("card body")
        .iter()
        .find(|e| e["type"] == "Image")
        .unwrap_or_else(|| panic!("expected an Image element; got: {card}"));
    let url = img["url"].as_str().expect("image url");
    assert!(
        url.starts_with("https://teams.example/msteams/img/"),
        "image points at the signed img route on the public base; got: {url}"
    );
    // The base64 PNG must never be dumped into the card text — a
    // render_report result is large and the Activity body is capped.
    assert!(
        !card.to_string().contains("png_base64"),
        "the inline base64 PNG must not leak into the card; got: {card}"
    );

    // Fetching the URL anonymously — no Activity, no JWT, exactly as
    // Teams does — renders on demand and returns the PNG bytes.
    let token = url.rsplit('/').next().expect("token in url");
    let img_resp = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{token}"))
        .send()
        .await
        .expect("GET img");
    assert!(img_resp.status().is_success(), "{}", img_resp.status());
    assert_eq!(
        img_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = img_resp.bytes().await.expect("png bytes");
    assert_eq!(
        &bytes[..4],
        b"\x89PNG",
        "expected PNG magic; got {} bytes",
        bytes.len()
    );

    // render_report is dispatched twice for a report reply now: once at reply
    // time to probe for a native Vega chart (interactive charts need the data
    // in the card — this fake report has none, so the card falls back to the
    // signed Image above), and once by the img route on fetch.
    assert_eq!(
        report.hits(),
        2,
        "reply-time native-chart probe + img-route fetch"
    );
}

/// The token is the only gate on an anonymous route, so a token minted
/// under a different key must not resolve — and must not reach
/// `render_report` either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_image_token_is_rejected() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(report_surface()).await;
    let report = FakeAgent::start_returning(peacock_png_result()).await;
    let proc = spawn_with_report_upstreams(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let payload = json!({
        "a": { "report_id": "supplier-concentration" },
        "exp": now_unix() + 600,
    });
    let forged =
        triton_correlation::encode_with_cap(IMG_MARKER, &payload, b"wrong-key", IMG_TOKEN_CAP)
            .expect("encode forged token");
    let resp = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{forged}"))
        .send()
        .await
        .expect("GET img forged");
    assert_eq!(resp.status(), 401, "a forged image token must 401");

    // A card-action token is NOT an image token: same key, different
    // marker slot, so it can't be replayed at the img route to drive an
    // arbitrary tool render.
    let replayed = triton_correlation::encode_with_cap(
        "render_report",
        &json!({ "report_id": "supplier-concentration" }),
        CORRELATION_KEY,
        IMG_TOKEN_CAP,
    )
    .expect("encode action token");
    let resp = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{replayed}"))
        .send()
        .await
        .expect("GET img replayed");
    assert_eq!(
        resp.status(),
        401,
        "a card-action token must not be replayable at the img route"
    );

    assert_eq!(
        report.hits(),
        0,
        "a refused token must never reach render_report"
    );
}

/// Image links carry an expiry so an old one eventually dies rather than
/// being a forever-capability. An expired token is `410 Gone` — distinct
/// from the 401 an unauthentic one gets, so a user scrolling ancient
/// history is told the link aged out, not that they were refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_image_token_is_gone() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(report_surface()).await;
    let report = FakeAgent::start_returning(peacock_png_result()).await;
    let proc = spawn_with_report_upstreams(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // Correctly signed, but minted in the past.
    let payload = json!({
        "a": { "report_id": "supplier-concentration" },
        "exp": now_unix() - 60,
    });
    let stale =
        triton_correlation::encode_with_cap(IMG_MARKER, &payload, CORRELATION_KEY, IMG_TOKEN_CAP)
            .expect("encode stale token");
    let resp = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{stale}"))
        .send()
        .await
        .expect("GET img stale");
    assert_eq!(
        resp.status(),
        410,
        "an expired image token must be 410 Gone"
    );
    assert_eq!(
        report.hits(),
        0,
        "an expired token must never reach render_report"
    );
}

/// The chart and the card chrome must resolve to the SAME peacock brand.
/// Peacock keys `brand` off the caller's tenant (`themes.resolve(&state
/// .principal.tenant, host)`), and Triton forwards `principal.tenant`
/// into the JWT it mints for an upstream, so the tenant the img route
/// dispatches under decides which brand themes the PNG. The route's
/// principal is synthetic — the render is authorized by the SIGNED token,
/// not by the fetcher — but it must still carry the tenant of the sender
/// the link was minted for, or a themed card ends up wrapping a
/// differently-branded chart. It is also what joins the render to that
/// tenant's activity in the audit pivot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn img_route_renders_under_the_senders_tenant() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(report_surface()).await;
    let report = FakeAgent::start_returning(peacock_png_result()).await;
    let proc = spawn_with_report_upstreams(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // Alice is `tenant: acme` in the fixture's sender_table.
    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("how concentrated are our suppliers?"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());
    let first = wait_for(Duration::from_secs(5), || {
        fake.captured().into_iter().next()
    });
    let url = first.body["attachments"][0]["content"]["body"]
        .as_array()
        .expect("card body")
        .iter()
        .find(|e| e["type"] == "Image")
        .and_then(|e| e["url"].as_str())
        .unwrap_or_else(|| panic!("expected an Image; got: {}", first.body))
        .to_string();
    let token = url.rsplit('/').next().expect("token in url");

    let img = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{token}"))
        .send()
        .await
        .expect("GET img");
    assert!(img.status().is_success(), "{}", img.status());

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "render_report"
    });
    assert_eq!(
        dispatch["tenant"], "acme",
        "the img render must run under the minting sender's tenant, not a \
         placeholder — peacock brands the chart off it; got: {dispatch}"
    );
}

/// Image links live 7 days, so a token minted before the tenant field
/// existed is still in the wild when this ships. It must still render —
/// falling back to the old placeholder tenant — rather than 401 and
/// break every chart already sitting in a Teams conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_minted_before_the_tenant_field_still_renders() {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(report_surface()).await;
    let report = FakeAgent::start_returning(peacock_png_result()).await;
    let proc = spawn_with_report_upstreams(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // The pre-#200 payload shape: args + expiry, no `t`.
    let legacy = triton_correlation::encode_with_cap(
        IMG_MARKER,
        &json!({
            "a": { "report_id": "supplier-concentration" },
            "exp": now_unix() + 600,
        }),
        CORRELATION_KEY,
        IMG_TOKEN_CAP,
    )
    .expect("encode legacy token");

    let resp = reqwest::Client::new()
        .get(format!("http://{webhook}/msteams/img/{legacy}"))
        .send()
        .await
        .expect("GET img legacy");
    assert!(
        resp.status().is_success(),
        "a pre-tenant token must still render; got {}",
        resp.status()
    );
    let bytes = resp.bytes().await.expect("png bytes");
    assert_eq!(&bytes[..4], b"\x89PNG");

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "render_report"
    });
    assert_eq!(
        dispatch["tenant"], "-",
        "a tenant-less token keeps the old placeholder; got: {dispatch}"
    );
}

fn wait_for_audit(
    proc: &TritonProcess,
    deadline: Duration,
    matches: impl Fn(&Value) -> bool,
) -> Value {
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
