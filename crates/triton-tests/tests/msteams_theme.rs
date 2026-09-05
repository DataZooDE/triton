//! Issue #200 — peacock `get_theme` chrome on Microsoft Teams Adaptive Cards.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256 JWT
//! verification against the in-repo `FakeBotFramework` fixture, and a
//! real `get_theme` upstream reached over TCP through the static
//! upstream router — the same wire path peacock sits on in production
//! (`X-Triton-Tool: get_theme`, peacock#18 `mcp::get_theme`).
//!
//! Peacock owns ALL theming: one CSS of `--pk-*` tokens themes charts,
//! iframes AND this card chrome. The adapter carries no theme config;
//! it consumes the resolved values. So the matrix is:
//!
//!  * `get_theme` registered + branded → the card leads with a themed
//!    header (logo + title), avatar or banner per `logo_style`.
//!  * no `get_theme` upstream → today's unbranded card, unchanged.
//!
//! `brand_color` is deliberately NOT consumed: Adaptive Cards carry no
//! arbitrary colour, and Teams renders light/dark/high-contrast themes
//! the card cannot detect (see `doc/realizations.md` §7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-test.yaml")
        .display()
        .to_string()
}

fn env_with(fake: &FakeBotFramework) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
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

/// What peacock's `get_theme` returns for a BRANDED deployment
/// (`PEACOCK_BRAND_CSS` set): `--pk-name` → title, `--pk-logo` → logo_url,
/// `--pk-logo-style` → logo_style. Shape verbatim from peacock's
/// `mcp::get_theme` (peacock#18).
fn branded_theme(logo_style: &str) -> Value {
    json!({
        "brand": "acme", "host": "teams.example",
        "title": "DataZoo Supplier Risk",
        "logo_url": "https://brand.example/logo.png",
        "logo_style": logo_style,
        "brand_color": "#0e7a5f",
        "accent": "#14b58c",
        "css": ":root { --pk-brand: #0e7a5f; }",
    })
}

/// Drive `/narrate alice` (Text + Narration + Button ⇒ an Adaptive Card
/// reply) and return the card `content` the adapter POSTed back through
/// the bot connector.
async fn card_for(env: HashMap<String, String>, fake: &FakeBotFramework) -> Value {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(good_claims(fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("/narrate alice"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());
    let first = wait_for(Duration::from_secs(5), || {
        fake.captured().into_iter().next()
    });
    let card = first.body["attachments"][0]["content"].clone();
    assert_eq!(
        card["type"], "AdaptiveCard",
        "reply must be an Adaptive Card; got: {}",
        first.body
    );
    card
}

/// A branded `get_theme` in `avatar` mode leads the card with a themed
/// header: the round logo beside the brand title, in an `emphasis`
/// container. The narrate text follows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_theme_brands_the_card_header_avatar() {
    let fake = FakeBotFramework::start().await;
    let theme = FakeAgent::start_returning(branded_theme("avatar")).await;
    let mut env = env_with(&fake);
    env.insert(
        "TRITON_STATIC_UPSTREAMS".to_string(),
        format!("get_theme={}", theme.host_port()),
    );

    let card = card_for(env, &fake).await;

    let header = &card["body"][0];
    assert_eq!(
        header["type"], "Container",
        "themed card leads with a header container; got: {card}"
    );
    assert_eq!(
        header["style"], "emphasis",
        "header uses the host-themed emphasis band (Adaptive Cards carry no \
         arbitrary colour); got: {header}"
    );
    let cols = &header["items"][0];
    assert_eq!(
        cols["type"], "ColumnSet",
        "avatar mode = logo column beside title column; got: {header}"
    );
    let logo = &cols["columns"][0]["items"][0];
    assert_eq!(logo["type"], "Image");
    assert_eq!(logo["url"], "https://brand.example/logo.png");
    assert_eq!(
        logo["style"], "Person",
        "avatar mode ⇒ round logo; got: {logo}"
    );
    assert_eq!(
        cols["columns"][1]["items"][0]["text"], "DataZoo Supplier Risk",
        "header carries --pk-name; got: {header}"
    );

    // The answer still follows the header, and the button still works.
    let rest = card["body"].to_string();
    assert!(
        rest.contains("Hello, alice."),
        "the answer must survive the header; got: {card}"
    );
    assert!(
        card["actions"][0]["data"]["ct"].is_string(),
        "the signed action must survive the header; got: {card}"
    );
}

/// `logo_style: banner` (a wide wordmark the round avatar slot would
/// crop) renders the logo full-width above the title instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_theme_banner_logo_renders_full_width() {
    let fake = FakeBotFramework::start().await;
    let theme = FakeAgent::start_returning(branded_theme("banner")).await;
    let mut env = env_with(&fake);
    env.insert(
        "TRITON_STATIC_UPSTREAMS".to_string(),
        format!("get_theme={}", theme.host_port()),
    );

    let card = card_for(env, &fake).await;

    let header = &card["body"][0];
    assert_eq!(header["type"], "Container", "got: {card}");
    let logo = &header["items"][0];
    assert_eq!(
        logo["type"], "Image",
        "banner mode ⇒ a bare Image, no ColumnSet; got: {header}"
    );
    assert_eq!(logo["url"], "https://brand.example/logo.png");
    assert_eq!(
        logo["size"], "Stretch",
        "banner mode ⇒ full-width logo; got: {logo}"
    );
    assert_eq!(
        header["items"][1]["text"], "DataZoo Supplier Risk",
        "title sits under the banner; got: {header}"
    );
}

/// The regression guard for "unset theme = today's unbranded rendering":
/// with no `get_theme` upstream registered the dispatch fails, the
/// adapter degrades to the default chrome, and the card is exactly what
/// it was before theming existed — the answer is the first body element.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_get_theme_upstream_leaves_the_card_unbranded() {
    let fake = FakeBotFramework::start().await;
    let card = card_for(env_with(&fake), &fake).await;

    assert_eq!(
        card["body"][0]["type"], "TextBlock",
        "unbranded card leads with the answer, no header container; got: {card}"
    );
    assert!(
        card["body"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Hello, alice."),
        "got: {card}"
    );
    assert!(
        card["body"]
            .as_array()
            .expect("body")
            .iter()
            .all(|e| e["type"] != "Container"),
        "no theme ⇒ no header container anywhere; got: {card}"
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
