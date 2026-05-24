//! `POST /v1/surface/render` — runs an A2UI Surface through a
//! chat-channel surface mapper and returns what that adapter would
//! post. Today only `adapter=telegram` is wired.
//!
//! Acceptance:
//!   * Happy path returns `{rendered: true, text, parse_mode,
//!     deferred_buttons, truncated}`.
//!   * Empty-after-render returns `{rendered: false, reason}`.
//!   * Unknown adapter is 400 Validation.
//!   * Non-A2UI result body is 400 Validation.
//!   * Auth required.
//!
//! No mocks: real binary, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

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
async fn telegram_render_counts_deferred_buttons() {
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
    assert_eq!(body["deferred_buttons"], 1);
    let text = body["text"].as_str().expect("text str");
    assert!(text.contains("label"));
    // Buttons defer — their label MUST NOT leak into the text body
    // (correlation-token PR will eventually emit them as inline
    // keyboard buttons; for now they're invisible).
    assert!(
        !text.contains("Refresh"),
        "deferred button label should not appear in text: `{text}`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_render_reports_empty_after_render() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .post(proc.rest_url("/v1/surface/render"))
        .bearer_auth("dev-token")
        .json(&serde_json::json!({
            "adapter": "telegram",
            // Button-only surface → no usable text → EmptyAfterRender.
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
    assert_eq!(body["rendered"], false);
    assert_eq!(body["reason"], "empty_after_render");
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
