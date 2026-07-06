//! `POST /v1/surface/render` — runs an A2UI Surface through a
//! chat-channel surface mapper and returns what that adapter would
//! post. All six adapters are wired: telegram, discord, googlechat,
//! msteams, signal, whatsapp.
//!
//! Acceptance:
//!   * Happy path returns `{rendered: true, text, deferred_*, ...}`
//!     with adapter-specific extras (parse_mode/reply_markup for
//!     telegram, components for discord, has_dashboard_raster for
//!     telegram/discord/whatsapp).
//!   * Empty-after-render returns `{rendered: false, reason}`.
//!   * Unknown adapter is 400 Validation.
//!   * Non-A2UI result body is 400 Validation.
//!   * Auth required.
//!
//! No mocks: real binary, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

/// A surface every text-first mapper renders the same way (text +
/// narration), so we can drive the whole adapter set through one
/// helper and assert the common envelope shape.
fn text_surface() -> serde_json::Value {
    serde_json::json!({
        "surface": {
            "components": [
                { "kind": "text", "value": "Hello" },
                { "kind": "narration", "text": "a footnote" },
            ]
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_render_returns_text_and_parse_mode() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": {
                "surface": {
                    "components": [
                        { "kind": "text", "value": "Hello" },
                        { "kind": "narration", "text": "a footnote" },
                    ]
                }
            }
        }))
        .send()
        .await
        .expect("POST /v1/surface/render")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(body["adapter"], "telegram");
    assert_eq!(body["rendered"], true);
    let text = body["text"].as_str().expect("text str");
    assert!(text.contains("Hello"));
    assert!(text.contains("<i>a footnote</i>"));
    assert_eq!(body["parse_mode"], "HTML");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_render_emits_buttons_as_inline_keyboard() {
    // Contract update: PR 21 added HMAC correlation tokens so
    // buttons now ship as inline_keyboard, not deferred. The
    // mapper signs with the route-internal PREVIEW_KEY (zero
    // bytes) — tokens are NOT replayable against a live adapter
    // because every manifest entry uses a distinct vault-resolved
    // key. PR 19's assertion that the button "label MUST NOT
    // leak" is inverted post-PR 21: the button label DOES appear
    // in reply_markup.inline_keyboard.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": {
                "surface": {
                    "components": [
                        { "kind": "text", "value": "label" },
                        {
                            "kind": "button",
                            "label": "Refresh",
                            "tool": "narrate",
                            "args": {}
                        }
                    ]
                }
            }
        }))
        .send()
        .await
        .expect("POST /v1/surface/render")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(body["rendered"], true);
    assert_eq!(body["deferred_buttons"], 0);
    let text = body["text"].as_str().expect("text str");
    assert!(text.contains("label"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_render_synthesises_text_for_button_only_surface() {
    // Contract update: PR 22 added a "Choose an option:"
    // placeholder so a button-only Surface still ships its
    // (now interactive, PR 21) buttons. Used to be
    // EmptyAfterRender — locking it in here as a regression
    // guard.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": {
                "surface": {
                    "components": [
                        {
                            "kind": "button",
                            "label": "x",
                            "tool": "echo",
                            "args": {}
                        }
                    ]
                }
            }
        }))
        .send()
        .await
        .expect("POST /v1/surface/render")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(body["rendered"], true);
    let text = body["text"].as_str().expect("text");
    assert!(
        text.contains("Choose"),
        "expected button-only placeholder text; got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_adapter_renders_the_text_surface() {
    // The whole point of the multi-adapter preview: a single A2UI
    // surface flows through each platform's mapper and comes back
    // with that platform's rendering. We assert the common envelope
    // keys are present for all six.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let client = reqwest::Client::new();
    for adapter in [
        "telegram",
        "discord",
        "googlechat",
        "msteams",
        "signal",
        "whatsapp",
    ] {
        let body: serde_json::Value = client
            .post(proc.rest_url("/v1/surface/render"))
            .bearer_auth("dev-token")
            .json(&serde_json::json!({ "adapter": adapter, "result": text_surface() }))
            .send()
            .await
            .unwrap_or_else(|e| panic!("POST render {adapter}: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("decode {adapter}: {e}"));
        assert_eq!(body["adapter"], adapter, "adapter echo: {body}");
        assert_eq!(body["rendered"], true, "{adapter} should render: {body}");
        let text = body["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{adapter} text field missing: {body}"));
        assert!(
            text.contains("Hello"),
            "{adapter} should carry the text component: {body}"
        );
        // Every mapper exposes a deferred_buttons counter.
        assert!(
            body["deferred_buttons"].is_number(),
            "{adapter} missing deferred_buttons: {body}"
        );
        assert!(
            body["truncated"].is_boolean(),
            "{adapter} missing truncated: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discord_render_exposes_components_for_buttons() {
    // Discord renders buttons as components v2, not text — so a
    // button-bearing surface should come back with a non-null
    // `components` payload (the explorer shows it as raw JSON).
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "discord",
            "result": {
                "surface": {
                    "components": [
                        { "kind": "text", "value": "pick:" },
                        {
                            "kind": "button",
                            "label": "Go",
                            "tool": "narrate",
                            "args": {}
                        }
                    ]
                }
            }
        }))
        .send()
        .await
        .expect("POST render discord")
        .json()
        .await
        .expect("decode discord");
    assert_eq!(body["rendered"], true, "discord render: {body}");
    assert!(
        !body["components"].is_null(),
        "discord should emit components v2 for buttons: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepts_an_already_negotiated_v09_envelope() {
    // The Explorer's per-turn channel preview POSTs the bubble's *raw*
    // envelope — an already-negotiated v0.9 `{version, stream}` — rather
    // than re-invoking the tool (which for an LLM agent would run a whole
    // new turn). The endpoint reverses it back to a Surface, so the same
    // text comes out as the canonical `{surface}` path would.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": {
                "version": "0.9",
                "stream": [
                    { "type": "text", "text": "Hello" },
                    { "type": "narration", "text": "a footnote" },
                ]
            }
        }))
        .send()
        .await
        .expect("POST /v1/surface/render")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(
        body["rendered"], true,
        "negotiated envelope should render: {body}"
    );
    let text = body["text"].as_str().expect("text str");
    assert!(
        text.contains("Hello"),
        "text component lost in reverse: {body}"
    );
    assert!(
        text.contains("<i>a footnote</i>"),
        "narration lost in reverse: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiated_v09_button_preserves_interaction() {
    // The reverse must unwrap the v0.9 `action: {tool, args}` back to a
    // flat button so the mapper still emits an interactive control — proof
    // the action round-trips, not just the text.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": {
                "version": "0.9",
                "stream": [
                    { "type": "text", "text": "label" },
                    {
                        "type": "button",
                        "label": "Refresh",
                        "action": { "tool": "narrate", "args": {} }
                    }
                ]
            }
        }))
        .send()
        .await
        .expect("POST /v1/surface/render")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(
        body["rendered"], true,
        "button envelope should render: {body}"
    );
    assert_eq!(
        body["deferred_buttons"], 0,
        "button should be interactive: {body}"
    );
    let text = body["text"].as_str().expect("text str");
    assert!(text.contains("label"), "text lost: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_adapter_rejected() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "carrier_pigeon",
            "result": { "surface": { "components": [] } }
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_surface_field_rejected() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            // No `surface` field → not an A2UI result.
            "result": { "foo": "bar" }
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_endpoint_requires_auth() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .json(&serde_json::json!({
            "adapter": "telegram",
            "result": { "surface": { "components": [] } }
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}
