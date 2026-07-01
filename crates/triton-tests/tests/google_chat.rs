//! v0.2 PR 33 — Google Chat adapter integration tests.
//!
//! Drives the full inbound flow against the real binary:
//!   * Google-signed JWT verification (RS256, real PEM cert served
//!     by `FakeGoogleJwks` on a real TCP port).
//!   * Sender resolution against the `sender_table`.
//!   * Inline response — Google Chat's synchronous-response pattern
//!     means the HTTP 200 body of the webhook IS the bot's reply.
//!   * Non-MESSAGE event types acked without dispatch.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real
//! RS256-signed JWT against a real PEM-wrapped X.509 cert.
//!
//! Fixture: `crates/triton-tests/src/chat_courier_fixture.rs`'s
//! `FakeGoogleJwks` ships a pre-generated RSA-2048 keypair so
//! `cargo test --workspace` is deterministic across runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::{FakeGoogleJwks, attacker_signing_key};
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "1234567890";
const GOOGLE_ISS: &str = "chat@system.gserviceaccount.com";
/// Issuer of the token the **current** Google Chat console sends — a
/// standard Google OIDC ID token (#134), not the legacy service-account
/// flavor above.
const GOOGLE_OIDC_ISS: &str = "https://accounts.google.com";
/// The Chat platform service account Google stamps into the `email`
/// claim of the OIDC-flavor token — the discriminator that proves the
/// token was minted for Chat and not by some other Google caller.
const CHAT_PLATFORM_SA: &str = "chat@system.gserviceaccount.com";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-google-chat-test.yaml")
        .display()
        .to_string()
}

fn env_with(jwks: &FakeGoogleJwks) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
    ])
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn standard_claims() -> Value {
    let now = unix_now();
    json!({
        "iss": GOOGLE_ISS,
        "aud": AUDIENCE,
        "iat": now,
        "exp": now + 600,
        "sub": "google-chat-svc",
    })
}

/// Standard MESSAGE event body. Mirror of the Google Chat
/// developer-docs payload shape; only the fields the adapter
/// reads are populated.
fn message_event(sender_name: &str, text: &str) -> Value {
    json!({
        "type": "MESSAGE",
        "eventTime": "2026-05-25T10:00:00.000Z",
        "message": {
            "name": "spaces/AAA/messages/BBB",
            "sender": { "name": sender_name, "displayName": "Alice", "type": "HUMAN" },
            "text": text,
            "space": { "name": "spaces/AAA", "type": "DM" }
        },
        "space": { "name": "spaces/AAA", "type": "DM" },
        "user": { "name": sender_name }
    })
}

/// A `CARD_CLICKED` interaction event carrying the signed correlation
/// token in `common.parameters.ct` (the shape a clicked Cards v2 button
/// produces). `user.name` is the clicker — the adapter resolves identity
/// off it just like `message.sender` on a MESSAGE.
fn card_clicked_event(sender_name: &str, token: &str) -> Value {
    json!({
        "type": "CARD_CLICKED",
        "user": { "name": sender_name },
        // `lbl` is the button's display label, echoed back so the reply can
        // show which button was tapped.
        "common": { "parameters": { "ct": token, "lbl": "Alpine Scorecard" } },
        "space": { "name": "spaces/AAA", "type": "DM" }
    })
}

/// Sign `(tool, args)` into a button correlation token under the test
/// manifest's `correlation_key`, exactly as the adapter does when it
/// renders a button.
fn sign_button(tool: &str, args: Value) -> String {
    triton_correlation::encode_with_cap(tool, &args, b"correlation-key-for-test", 1536)
        .expect("encode correlation token")
}

/// A `CARD_CLICKED` from a Selection/Form submit: the signed token plus the
/// user's `formInputs` (the chosen/typed values, keyed by widget name).
fn card_clicked_form_event(sender_name: &str, token: &str, inputs: &[(&str, &str)]) -> Value {
    let form_inputs: serde_json::Map<String, Value> = inputs
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                json!({ "stringInputs": { "value": [value] } }),
            )
        })
        .collect();
    json!({
        "type": "CARD_CLICKED",
        "user": { "name": sender_name },
        "common": { "parameters": { "ct": token }, "formInputs": form_inputs },
        "space": { "name": "spaces/AAA", "type": "DM" }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_jwt_message_dispatches_inline_response() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "hello from google chat"))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    let body: Value = resp.json().await.expect("response body");
    // Echo just returns its `message` arg under the same key; the
    // adapter's bare-text path renders it as `text: "<msg>"`.
    let text = body["text"].as_str().expect("text in inline response");
    assert!(
        text.contains("hello from google chat"),
        "expected echo of the inbound text; got: {text}"
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["tenant"], "acme");
    assert_eq!(audit["result"], "ok");

    // The inline-response delivery is audited as `phase: post` with
    // status_label `posted` (latency_ms = 0 because there's no
    // outbound roundtrip).
    let post = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(post["status_label"], "posted");
    assert_eq!(post["result"], "ok");
}

/// A `CARD_CLICKED` carrying a valid correlation token re-invokes the
/// signed `(tool, args)` — the interactive button round-trip. The token
/// is signed under the same `correlation_key` the adapter renders with,
/// so it verifies; the click re-dispatches `echo` with the button's args
/// and the answer comes back inline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_clicked_valid_token_redispatches() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let jwt = jwks.sign_jwt(standard_claims());
    let token = sign_button("echo", json!({ "message": "from a click" }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&card_clicked_event("users/99", &token))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Re-dispatched: echo replies with the button's args.
    let body: Value = resp.json().await.expect("response body");
    assert!(
        body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("from a click"),
        "expected the re-dispatched echo answer; got: {body}"
    );
    // The reply leads with the tapped button's label so chat history shows
    // which of several buttons was clicked.
    assert!(
        body["text"]
            .as_str()
            .unwrap_or_default()
            .starts_with("*↳ Alpine Scorecard*"),
        "reply should echo the tapped button label; got: {body}"
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["result"], "ok");
}

/// A `CARD_CLICKED` whose correlation token doesn't verify under the
/// adapter's `correlation_key` MUST be rejected — otherwise a crafted
/// click that reaches the (authenticated) webhook could invoke an
/// arbitrary tool with arbitrary args.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_clicked_forged_token_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let jwt = jwks.sign_jwt(standard_claims());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        // Well-formed envelope, but the token is not signed by our key.
        .json(&card_clicked_event("users/99", "Zm9yZ2Vk.Zm9yZ2Vk"))
        .send()
        .await
        .expect("POST webhook");
    assert_eq!(
        resp.status(),
        401,
        "a CARD_CLICKED with an unverifiable correlation token must be rejected"
    );

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

/// A Selection/Form submit: the `CARD_CLICKED` carries a token signed with
/// EMPTY base args plus the user's `formInputs`. The adapter verifies the
/// token (fixing the tool), merges the form values onto the args, and
/// re-dispatches — so the typed/selected value reaches the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_clicked_form_submit_merges_inputs() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let jwt = jwks.sign_jwt(standard_claims());
    // A form/selection signs the tool with empty base args; the value
    // arrives in formInputs.
    let token = sign_button("echo", json!({}));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&card_clicked_form_event(
            "users/99",
            &token,
            &[("message", "typed in the form")],
        ))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    // echo received the merged form value.
    let body: Value = resp.json().await.expect("response body");
    assert!(
        body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("typed in the form"),
        "expected the form input merged into the dispatch; got: {body}"
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["result"], "ok");
}

/// A preset button tapped on a card that ALSO has a (blank) form field:
/// Google submits every input, so the click carries an EMPTY formInput.
/// That empty value must NOT clobber the button's own signed `message`
/// arg — the button still drives its preset, not a blanked-out one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_clicked_empty_form_input_does_not_clobber_button_args() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let jwt = jwks.sign_jwt(standard_claims());
    // A button signs its full preset args; the co-located form field is
    // empty and shares the `message` key.
    let token = sign_button("echo", json!({ "message": "the button preset" }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&card_clicked_form_event(
            "users/99",
            &token,
            &[("message", "")],
        ))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    let body: Value = resp.json().await.expect("response body");
    assert!(
        body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("the button preset"),
        "empty form input must not clobber the button's preset arg; got: {body}"
    );
}

/// The chart-image route: `GET …/img/{token}` decodes the signed dashboard
/// spec and returns a rasterised PNG. Google fetches this URL anonymously
/// (no Chat JWT), so the signed token is the only gate — a token signed
/// with our key resolves; the response is `image/png`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_image_route_returns_a_png() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let spec = json!({
        "title": "Stock at risk (€)",
        "tiles": [
            { "label": "Alpine Metals AG", "value": "€2.19M" },
            { "label": "Catalonia Carbon", "value": "€1.79M" }
        ]
    });
    let token = triton_correlation::encode_with_cap(
        "__dashboard_png",
        &spec,
        b"correlation-key-for-test",
        8192,
    )
    .expect("encode dashboard token");

    let resp = reqwest::Client::new()
        .get(format!("http://{webhook}/google_chat/img/{token}"))
        .send()
        .await
        .expect("GET img");
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = resp.bytes().await.expect("png bytes");
    // PNG magic number.
    assert_eq!(
        &bytes[..4],
        b"\x89PNG",
        "expected a PNG; got {} bytes",
        bytes.len()
    );

    // A token signed with a DIFFERENT key must not resolve (404).
    let forged = triton_correlation::encode_with_cap("__dashboard_png", &spec, b"wrong-key", 8192)
        .expect("encode");
    let resp2 = reqwest::Client::new()
        .get(format!("http://{webhook}/google_chat/img/{forged}"))
        .send()
        .await
        .expect("GET img forged");
    assert_eq!(resp2.status(), 404);
}

/// The tiniest valid PNG (1×1), base64 — as an upstream returns it inline.
const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAP8/xJ8oAAAAAElFTkSuQmCC";

/// A `render_report`-style upstream (peacock) returns its chart as an inline
/// base64 PNG at `structuredContent.result._meta.png_base64`, plus components
/// (kpi/vega/table) this adapter can't map to Cards v2. On a `CARD_CLICKED`
/// the adapter must (a) serve that PNG as a Cards v2 **image** widget via the
/// signed `…/img/` route, and (b) caption it with just the tapped label — NOT
/// dump the whole (huge) result JSON as text, which Google rejects as
/// "message too long". Regression test for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_clicked_render_report_png_renders_as_image() {
    let jwks = FakeGoogleJwks::start().await;
    // The upstream `render_report` returns a peacock-shaped artifact: a
    // multi-key object (so the adapter's text fallback would serialise the
    // whole thing) carrying the PNG in `_meta`.
    let upstream = FakeAgent::start_returning(json!({
        "isError": false,
        "content": [{ "type": "text", "text": "Supplier concentration" }],
        "structuredContent": { "result": { "_meta": { "png_base64": TINY_PNG_B64 } } }
    }))
    .await;
    let mut env = env_with(&jwks);
    env.insert(
        "TRITON_STATIC_UPSTREAMS".to_string(),
        format!("render_report={}", upstream.host_port()),
    );
    // A reachable public base so the adapter mints an image URL (else it would
    // fall back to text).
    env.insert(
        "TRITON_GOOGLE_CHAT_PUBLIC_BASE".to_string(),
        "https://chat.example".to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Click the "concentration chart" button: a token routing to render_report.
    let token = sign_button(
        "render_report",
        json!({ "report_id": "supplier-concentration" }),
    );
    let jwt = jwks.sign_jwt(standard_claims());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&card_clicked_event("users/99", &token))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());
    let body: Value = resp.json().await.expect("json reply");

    // The reply is a Cards v2 image card pointing at the /img route.
    let img_url = body["cardsV2"][0]["card"]["sections"][0]["widgets"][0]["image"]["imageUrl"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an image widget, got: {body}"));
    assert!(
        img_url.contains("/google_chat/img/"),
        "image points at the signed /img route: {img_url}"
    );
    // The caption is short — the huge result JSON must NOT be dumped as text.
    let text = body["text"].as_str().unwrap_or_default();
    assert!(
        text.len() < 200,
        "caption must be short, got {} bytes",
        text.len()
    );
    assert!(
        !text.contains("png_base64"),
        "must not dump the base64 PNG into the message text"
    );

    // The /img route serves the actual PNG bytes.
    let img_token = img_url.rsplit('/').next().expect("token in url");
    let img = reqwest::Client::new()
        .get(format!("http://{webhook}/google_chat/img/{img_token}"))
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
    assert_eq!(
        &bytes[..4],
        b"\x89PNG",
        "expected PNG magic, got {} bytes",
        bytes.len()
    );
}

/// #134: a token from the **current** Google Chat console — a standard
/// Google OIDC ID token (`iss = https://accounts.google.com`, `aud =`
/// the App URL, keys from Google's OIDC JWKS) — MUST be accepted,
/// exactly like the legacy service-account token. This drives the
/// realistic OIDC path end to end: the App-URL audience and the
/// JWKS-shaped `oauth2/v3/certs` keyset (not the legacy x509 cert-map).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_issuer_jwt_message_dispatches_inline_response() {
    // App URL the modern Chat console uses as the token `aud`; matches
    // `inbound.audience` in the OIDC manifest fixture.
    const OIDC_AUDIENCE: &str = "https://triton.example.com/google_chat/webhook";
    let oidc_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-google-chat-oidc-test.yaml")
        .display()
        .to_string();

    let jwks = FakeGoogleJwks::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), oidc_manifest),
        // Point at the JWKS-shaped certs endpoint — the real OIDC source.
        (
            "TRITON_GOOGLE_CHAT_JWKS_URI".to_string(),
            jwks.oidc_jwks_uri(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let now = unix_now();
    let token = jwks.sign_jwt(json!({
        "iss": GOOGLE_OIDC_ISS,
        "aud": OIDC_AUDIENCE,
        "iat": now,
        "exp": now + 600,
        "sub": "1029384756",
        // Google stamps the Chat platform service account here; it is
        // the only Chat-specific proof on the OIDC flavor (the issuer
        // is shared by every Google ID token).
        "email": CHAT_PLATFORM_SA,
        "email_verified": true,
    }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "hello from the OIDC console"))
        .send()
        .await
        .expect("POST webhook");
    assert!(
        resp.status().is_success(),
        "OIDC-issuer token must be accepted (#134), got {}",
        resp.status()
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["result"], "ok");
}

/// A Chat app deployed as a **Google Workspace Add-on** signs its
/// webhook with the Google-managed Workspace Add-ons service agent
/// (`service-<project>@gcp-sa-gsuiteaddons.iam.gserviceaccount.com`)
/// rather than the legacy `chat@system` SA. That namespace is
/// google.com-owned — only Google can sign as it — so it is just as
/// unforgeable an actor proof. #141 only knew `chat@system` and rejected
/// this token; it MUST now be accepted (real captured flavor).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_token_from_workspace_addon_actor_is_accepted() {
    const OIDC_AUDIENCE: &str = "https://triton.example.com/google_chat/webhook";
    let oidc_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-google-chat-oidc-test.yaml")
        .display()
        .to_string();

    let jwks = FakeGoogleJwks::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), oidc_manifest),
        (
            "TRITON_GOOGLE_CHAT_JWKS_URI".to_string(),
            jwks.oidc_jwks_uri(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let now = unix_now();
    let token = jwks.sign_jwt(json!({
        "iss": GOOGLE_OIDC_ISS,
        "aud": OIDC_AUDIENCE,
        "iat": now,
        "exp": now + 600,
        "sub": "1029384756",
        // Workspace Add-ons service agent — the actor Google stamps for a
        // Chat app deployed as a Workspace Add-on.
        "email": "service-190449745291@gcp-sa-gsuiteaddons.iam.gserviceaccount.com",
        "email_verified": true,
    }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "hello from a workspace add-on"))
        .send()
        .await
        .expect("POST webhook");
    assert!(
        resp.status().is_success(),
        "Workspace Add-on actor token must be accepted, got {}",
        resp.status()
    );

    // The reply MUST use the Workspace Add-on envelope — a bare `{text}`
    // is rejected by the add-on runtime ("Cannot find field: text in
    // RenderActions/DataActions/Card"). The message text rides inside
    // hostAppDataAction → chatDataAction → createMessageAction.
    let body: Value = resp.json().await.expect("response body");
    assert!(
        body.get("text").is_none(),
        "add-on reply must NOT be a bare {{text}}; got: {body}"
    );
    let reply_text = body["hostAppDataAction"]["chatDataAction"]["createMessageAction"]["message"]
        ["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected add-on createMessageAction envelope; got: {body}"));
    assert!(
        reply_text.contains("hello from a workspace add-on"),
        "expected the echoed text inside the add-on envelope; got: {reply_text}"
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["result"], "ok");
}

/// Codex security review (#141 follow-up): the OIDC issuer
/// `accounts.google.com` is shared by EVERY Google-minted ID token —
/// anyone with a Google service account can mint one via IAM
/// `generateIdToken` with `aud` = our PUBLIC App URL. Without a
/// Chat-specific discriminator, such a token would authenticate as the
/// Chat platform and let a forged body impersonate any enrolled sender.
/// Google's documented guard is the `email` claim: it MUST be the Chat
/// platform service account. A correctly-signed, correct-`aud` OIDC
/// token whose `email` is some other Google service account MUST be
/// rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_token_from_other_service_account_is_rejected() {
    const OIDC_AUDIENCE: &str = "https://triton.example.com/google_chat/webhook";
    let oidc_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-google-chat-oidc-test.yaml")
        .display()
        .to_string();

    let jwks = FakeGoogleJwks::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), oidc_manifest),
        (
            "TRITON_GOOGLE_CHAT_JWKS_URI".to_string(),
            jwks.oidc_jwks_uri(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Genuinely Google-signed (our fixture key stands in for Google's
    // OIDC key), correct issuer, correct App-URL audience — but minted
    // for the ATTACKER's service account, not the Chat platform.
    let now = unix_now();
    let token = jwks.sign_jwt(json!({
        "iss": GOOGLE_OIDC_ISS,
        "aud": OIDC_AUDIENCE,
        "iat": now,
        "exp": now + 600,
        "sub": "1029384756",
        "email": "attacker@evil-project.iam.gserviceaccount.com",
        "email_verified": true,
    }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        // Forged body claiming an enrolled sender.
        .json(&message_event("users/99", "impersonation attempt"))
        .send()
        .await
        .expect("POST webhook");
    assert_eq!(
        resp.status(),
        401,
        "OIDC token from a non-Chat service account must be rejected"
    );

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_jwt_signature_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // Sign with a DIFFERENT RSA keypair; the fixture cert won't
    // verify against this signature.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    // Use the fixture's published kid so the keyset lookup hits
    // the served cert; the cert's public key won't match the
    // attacker's private key, so RSA verify fails.
    header.kid = Some("triton-test-google-chat-key".to_string());
    let forged = jsonwebtoken::encode(&header, &standard_claims(), &attacker_signing_key())
        .expect("attacker signs");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {forged}"))
        .json(&message_event("users/99", "intruder"))
        .send()
        .await
        .expect("POST forged");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_jwt_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    // exp = 1 hour ago (well past the 5-minute leeway).
    let now = unix_now();
    let stale = json!({
        "iss": GOOGLE_ISS,
        "aud": AUDIENCE,
        "iat": now - 7200,
        "exp": now - 3600,
        "sub": "google-chat-svc",
    });
    let token = jwks.sign_jwt(stale);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "late"))
        .send()
        .await
        .expect("POST expired");
    assert_eq!(resp.status(), 401);
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:google_chat"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_audience_jwt_is_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let now = unix_now();
    let wrong_aud = json!({
        "iss": GOOGLE_ISS,
        "aud": "9999999999",
        "iat": now,
        "exp": now + 600,
        "sub": "google-chat-svc",
    });
    let token = jwks.sign_jwt(wrong_aud);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/99", "wrong aud"))
        .send()
        .await
        .expect("POST wrong-aud");
    assert_eq!(resp.status(), 401);
    let _ = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:google_chat"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        // users/77 is NOT in the sender_table.
        .json(&message_event("users/77", "who am I"))
        .send()
        .await
        .expect("POST unknown sender");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_message_event_types_silently_acked() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    let body = json!({
        "type": "ADDED_TO_SPACE",
        "eventTime": "2026-05-25T10:00:00.000Z",
        "space": { "name": "spaces/AAA", "type": "DM" },
        "user": { "name": "users/99" }
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("POST added_to_space");
    assert!(resp.status().is_success(), "{}", resp.status());

    let resp_body: Value = resp.json().await.expect("response body");
    // Empty object body — no `text` for Google to deliver.
    assert!(
        resp_body.as_object().map(|o| o.is_empty()).unwrap_or(false),
        "non-MESSAGE event MUST ack with empty body; got: {resp_body}"
    );

    // Wait a moment for any audit lines to flush, then assert no
    // dispatch / no rejected for this protocol. (Other non-google
    // audit lines are fine.)
    std::thread::sleep(Duration::from_millis(200));
    let any_google: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "audit" && v["protocol"] == "messenger:google_chat")
        .count();
    assert_eq!(
        any_google, 0,
        "ADDED_TO_SPACE MUST NOT produce any messenger:google_chat audit lines",
    );
}

// ---- self_enrol identity strategy (FR-I-7, M-ENROL-1) ------------

fn selfenrol_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-googlechat-selfenrol-test.yaml")
        .display()
        .to_string()
}

fn selfenrol_env(jwks: &FakeGoogleJwks) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            selfenrol_manifest_path(),
        ),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_enrol_unknown_sender_gets_pairing_principal_not_rejected() {
    // M-ENROL-1: an unknown sender (not in fallback_table) is NOT
    // rejected. First contact dispatches with the literal scope
    // "pairing" and a stable subject (the platform sender id). We
    // observe the pairing phase via the dispatch audit: who = sender
    // id, tenant = "pairing".
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), selfenrol_env(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/55", "hi, I'm new"))
        .send()
        .await
        .expect("POST webhook");
    assert!(
        resp.status().is_success(),
        "unknown sender under self_enrol must dispatch (pairing), not 401; got {}",
        resp.status()
    );

    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(
        dispatch["who"], "users/55",
        "pairing-phase subject = the platform sender id"
    );
    assert_eq!(dispatch["tenant"], "pairing", "pairing-phase tenant marker");
    assert_eq!(dispatch["result"], "ok");

    // It must NOT have produced a rejection audit.
    let rejections: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "rejected"
                && v["protocol"] == "messenger:google_chat"
        })
        .count();
    assert_eq!(
        rejections, 0,
        "unknown self_enrol sender must not be rejected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_enrol_enrolled_sender_gets_full_principal_same_subject() {
    // The flip side: a sender already present in fallback_table
    // yields a fully-scoped Principal. Subject is STILL the platform
    // sender id (same subject as the pairing phase would have used),
    // and tenant is the enrolled tenant, not "pairing".
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), selfenrol_env(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/77", "I'm enrolled"))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:google_chat"
    });
    assert_eq!(
        dispatch["who"], "users/77",
        "enrolled subject = the same platform sender id (stable across enrolment)"
    );
    assert_eq!(
        dispatch["tenant"], "acme",
        "enrolled tenant from fallback_table"
    );
    assert_eq!(dispatch["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_enrol_rejects_missing_or_nonuser_sender() {
    // Codex review finding 3: a MESSAGE whose sender.name is absent
    // (or is a non-`users/` actor such as a bot) cannot form a valid
    // Entra-style subject and MUST be rejected under self_enrol —
    // NOT admitted as a pairing principal with an empty/odd subject.
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), selfenrol_env(&jwks)).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());

    // (a) sender object present but no `name`.
    let mut no_name = message_event("users/x", "hi");
    no_name["message"]["sender"] = json!({ "displayName": "Anon", "type": "HUMAN" });
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&no_name)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401, "missing sender.name must be rejected");

    // (b) a non-`users/` actor (Google sends `bots/<id>` for bots).
    let bot = message_event("bots/42", "I am a bot");
    let resp2 = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&bot)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp2.status(), 401, "non-users/ sender must be rejected");

    let rejections = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:google_chat"
            && v["result"] == "error:auth"
    });
    let _ = rejections;
}

// ---- upstream identity strategy (FR-I-7) -------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_identity_resolves_principal_via_resolver_tool() {
    // FR-I-7 `upstream`: the adapter delegates sender resolution to a
    // resolver tool reached through the upstream dispatcher. A real
    // resolver agent (FakeAgent) returns {sub, scopes, tenant}; the
    // adapter then dispatches the actual command under that principal.
    let jwks = FakeGoogleJwks::start().await;
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat"],
        "tenant": "globex"
    }))
    .await;

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-googlechat-upstream-test.yaml")
        .display()
        .to_string();
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("resolve_identity={}", resolver.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/55", "hello via resolver"))
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The REAL command dispatch (tool = echo) must carry the
    // principal the resolver returned.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:google_chat"
            && v["tool"] == "echo"
    });
    assert_eq!(dispatch["who"], "resolved-bob", "sub from resolver tool");
    assert_eq!(dispatch["tenant"], "globex", "tenant from resolver tool");
    assert_eq!(dispatch["result"], "ok");

    // Prove the resolve actually traversed the upstream dispatcher
    // (not an in-process bypass): the resolver agent was hit exactly
    // once and saw the static upstream bearer, never the inbound token.
    assert_eq!(resolver.hits(), 1, "resolver agent must be called once");
    let bearers = resolver.bearers_seen();
    assert_eq!(bearers.len(), 1);
    assert_eq!(
        bearers[0], "dev-token",
        "resolver must receive the static upstream bearer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_identity_rejects_when_resolver_fails() {
    // If the resolver tool errors (or can't be reached), the inbound
    // is rejected — never dispatched with a guessed principal.
    let jwks = FakeGoogleJwks::start().await;
    let resolver = FakeAgent::start_always_failing().await;

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-googlechat-upstream-test.yaml")
        .display()
        .to_string();
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("resolve_identity={}", resolver.host_port()),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("listener bound");

    let token = jwks.sign_jwt(standard_claims());
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {token}"))
        .json(&message_event("users/55", "resolver will fail"))
        .send()
        .await
        .expect("POST webhook");
    assert_eq!(
        resp.status(),
        401,
        "resolver failure must reject the inbound"
    );

    // No `echo` dispatch must occur (only the resolver call, which
    // fails). Assert no real-command dispatch happened.
    std::thread::sleep(Duration::from_millis(300));
    let echo_dispatches = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:google_chat"
                && v["tool"] == "echo"
        })
        .count();
    assert_eq!(
        echo_dispatches, 0,
        "no command dispatch on resolver failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_resolver_colliding_with_inprocess_tool_refuses_to_boot() {
    // Codex review finding 1: a resolver_tool that names an in-process
    // tool (`echo`) would be dispatched locally, bypassing the upstream
    // dispatcher and its workload-to-workload token. The adapter MUST
    // refuse at boot. The collision check keys off the in-process tool
    // descriptors, so it fires regardless of upstream wiring.
    let bin = locate_triton_binary();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-googlechat-upstream-collision.yaml")
        .display()
        .to_string();
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest)
        .env("TRITON_GOOGLE_CHAT_JWKS_URI", "https://www.googleapis.com")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "resolver_tool colliding with an in-process tool MUST refuse to boot; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let cand = here.join("target/debug/triton");
        if cand.exists() {
            return cand;
        }
        here.pop();
    }
    panic!("triton binary not found");
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
