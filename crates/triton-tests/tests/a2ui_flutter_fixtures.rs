//! The v0.9 envelopes `packages/a2ui_flutter`'s widget tests render — produced
//! by a **real** Triton process wrapping a **real** upstream agent's surface,
//! not hand-written in Dart.
//!
//! Why this exists: the Flutter tests used to assert against literals a human
//! typed, and a `kind`/`type` mismatch between the two sides survived for weeks
//! because both sides agreed with themselves. A Dart literal proves the
//! renderer is self-consistent; it proves nothing about the wire. So the
//! fixtures under `packages/a2ui_flutter/test/fixtures/` are *generated* here —
//! FakeAgent returns a surface, a real `triton` binary wraps it at the
//! negotiated v0.9, and the response body is what lands on disk.
//!
//! The checked-in files are asserted byte-equal to a freshly produced envelope
//! on every CI run, so a change to `triton-core::a2ui::v09` that the Flutter
//! side has not been taught fails **here**, loudly, instead of being dropped
//! silently in a renderer.
//!
//! Regenerate with `UPDATE_A2UI_FIXTURES=1 cargo test -p triton-tests
//! --test it a2ui_flutter_fixtures` (the suite links as one `it` binary —
//! see `tests/it/main.rs`).
//!
//! No mocks: real binary, real HTTP, real JSON.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/a2ui_flutter/test/fixtures")
        .canonicalize()
        .expect("packages/a2ui_flutter/test/fixtures exists")
}

/// A surface composed **only** of the fields v0.9 carried before #206 grew the
/// vocabulary. This is the renderer's backward-compatibility control: whatever
/// the new fields do, an envelope like this one must render exactly as it did.
fn pre206_surface() -> Value {
    json!({
        "surface": { "components": [
            { "kind": "text", "value": "Filing on [[customer::hoffmann]] today" },
            { "kind": "button", "label": "Approve", "tool": "heron_approve", "args": { "id": "p1" } },
            { "kind": "form", "title": "Run skill", "submit_label": "Run", "tool": "heron_run",
              "fields": [
                { "name": "window", "label": "Window", "kind": "string", "required": false },
                { "name": "limit", "label": "Limit", "kind": "integer", "required": false },
                { "name": "dry", "label": "Dry run", "kind": "boolean", "required": true }
              ] },
            { "kind": "report", "report_id": "renewals", "args": { "window": "90d" } }
        ]}
    })
}

/// The same surface with every field #206 added set — the one the Flutter
/// renderer has to learn. Deliberately carries *both* a pilled and an unpilled
/// wikilink, and *both* a defaulted and a bare form field, so each degrade has
/// its control inside the same envelope.
fn extended_surface() -> Value {
    json!({
        "surface": { "components": [
            { "kind": "text",
              "value": "Filing on [[customer::hoffmann]] today, cc [[customer::secret]]",
              "pills": { "hoffmann": "Hoffmann Automotive" } },
            { "kind": "button", "label": "Approve", "tool": "heron_approve",
              "args": { "id": "p1" }, "primary": true },
            { "kind": "button", "label": "Reject", "tool": "heron_reject", "args": {} },
            { "kind": "form", "title": "Run skill", "submit_label": "Run", "tool": "heron_run",
              "fields": [
                { "name": "window", "label": "Window", "kind": "string", "required": false,
                  "placeholder": "e.g. 30d", "default": "30d" },
                { "name": "limit", "label": "Limit", "kind": "integer", "required": false,
                  "default": 25 },
                { "name": "dry", "label": "Dry run", "kind": "boolean", "required": true,
                  "default": true },
                { "name": "note", "label": "Note", "kind": "string", "required": false,
                  "placeholder": "optional note" }
              ] },
            { "kind": "report", "report_id": "renewals", "args": { "window": "90d" },
              "title": "Renewals", "series": [4.0, 8.0, 6.0],
              "labels": ["Q1", "Q2", "Q3"] }
        ]}
    })
}

/// A button that carries a `ui://` resource beside its tool call. Its own
/// fixture because a resource-bearing button suppresses a sibling inline
/// `report` — mixing the two would hide the preview under a placement rule.
fn resource_button_surface() -> Value {
    json!({
        "surface": { "components": [
            { "kind": "text", "value": "The renewals view is ready." },
            { "kind": "button", "label": "Open renewals", "tool": "render_report",
              "args": { "report_id": "renewals" },
              "resource": "ui://peacock/report/renewals?window=90d", "primary": true }
        ]}
    })
}

/// Round-trip one surface through a real upstream + a real gateway and return
/// the v0.9 envelope the gateway put on the wire.
async fn produced_v09_envelope(surface: Value) -> Value {
    let agent = FakeAgent::start_returning(surface).await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("heron={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/heron"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=0.9")
        .json(&json!({}))
        .send()
        .await
        .expect("POST /v1/tools/heron");
    assert!(resp.status().is_success(), "REST {}", resp.status());
    let body: Value = resp.json().await.expect("decode");
    let envelope = body["result"].clone();
    assert_eq!(
        envelope["version"], "0.9",
        "the gateway must have wrapped at v0.9: {body}"
    );
    envelope
}

/// Compare against the checked-in fixture, or rewrite it when explicitly asked.
fn pin(name: &str, produced: &Value) {
    let path = fixture_dir().join(name);
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(produced).expect("json")
    );
    if std::env::var("UPDATE_A2UI_FIXTURES").is_ok() {
        std::fs::write(&path, &text).expect("write fixture");
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — regenerate with UPDATE_A2UI_FIXTURES=1",
            path.display()
        )
    });
    assert_eq!(
        on_disk,
        text,
        "{} is stale: the v0.9 envelope Triton produces has changed, and the \
         Flutter renderer's tests are asserting against the old one. Inspect \
         the diff, teach packages/a2ui_flutter the change, then regenerate \
         with UPDATE_A2UI_FIXTURES=1.",
        path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_fixture_pre206_envelope_is_current() {
    let envelope = produced_v09_envelope(pre206_surface()).await;
    // The control the fixture exists to be: none of #206's fields appear when
    // the agent did not set them, so the Flutter test that renders this file is
    // genuinely rendering a pre-#206 envelope and not a defaulted one.
    let stream = envelope["stream"].as_array().expect("stream");
    assert!(stream[0].get("pills").is_none(), "{}", stream[0]);
    assert!(stream[1].get("primary").is_none(), "{}", stream[1]);
    assert!(stream[1].get("resource").is_none(), "{}", stream[1]);
    for f in stream[2]["fields"].as_array().expect("fields") {
        assert!(f.get("placeholder").is_none(), "{f}");
        assert!(f.get("default").is_none(), "{f}");
    }
    assert!(stream[3].get("title").is_none(), "{}", stream[3]);
    assert!(stream[3].get("series").is_none(), "{}", stream[3]);
    pin("v09_pre206.json", &envelope);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_fixture_extended_envelope_is_current() {
    let envelope = produced_v09_envelope(extended_surface()).await;
    let stream = envelope["stream"].as_array().expect("stream");
    // Positive control on the pills degrade: the map reached the wire, and the
    // id the caller may not read (D27) has no entry — absence is the signal the
    // renderer degrades on, so it must survive the round trip as absence.
    assert_eq!(stream[0]["pills"]["hoffmann"], "Hoffmann Automotive");
    assert!(stream[0]["pills"].get("secret").is_none(), "{}", stream[0]);
    assert_eq!(stream[1]["primary"], true);
    assert!(stream[2].get("primary").is_none(), "{}", stream[2]);
    let fields = stream[3]["fields"].as_array().expect("fields");
    assert_eq!(fields[0]["placeholder"], "e.g. 30d");
    assert_eq!(fields[0]["default"], "30d");
    // …and the last field is the control for the other half of the pair: a
    // placeholder with no default, which must stay hint text and never submit
    // itself as a value the agent never proposed.
    assert_eq!(fields[3]["placeholder"], "optional note");
    assert!(fields[3].get("default").is_none(), "{}", fields[3]);
    assert_eq!(stream[4]["title"], "Renewals");
    assert_eq!(stream[4]["series"][1], 8.0);
    pin("v09_extended.json", &envelope);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_fixture_resource_button_envelope_is_current() {
    let envelope = produced_v09_envelope(resource_button_surface()).await;
    let stream = envelope["stream"].as_array().expect("stream");
    assert_eq!(
        stream[1]["resource"], "ui://peacock/report/renewals?window=90d",
        "{}",
        stream[1]
    );
    // …and the tool call rides alongside it: a resource button is not a link,
    // it is an action that also names a view.
    assert_eq!(stream[1]["action"]["tool"], "render_report");
    pin("v09_resource_button.json", &envelope);
}
