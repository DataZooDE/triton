//! #95 — agent-initiated proactive dispatch API (`POST /v1/outbound`).
//!
//! A registered upstream agent submits an outbound message; Triton
//! resolves the named adapter's courier, renders the surface through
//! that adapter's surface mapper, posts to the platform, and audits a
//! `phase: post` record sharing the response `trace_id`.
//!
//! Auth is a bearer carrying a DEDICATED outbound audience
//! (`TRITON_OUTBOUND_AUDIENCE`), distinct from the HTTP-trio audience
//! (`TRITON_OIDC_AUDIENCE`). A token minted for the trio audience must
//! NOT be accepted on `/v1/outbound`, and vice-versa.
//!
//! No mocks: real binary, real OIDC issuer (Ed25519 JWKS), real HTTP
//! to the in-repo `FakeWhatsAppApi` that speaks the Cloud API wire
//! shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::{TestIssuer, TritonProcess};

const ACCESS_TOKEN: &str = "whatsapp-access-token-for-test";
const PHONE_NUMBER_ID: &str = "100200300";
const KNOWN_WA_ID: &str = "491701234567";
const TRIO_AUDIENCE: &str = "agents-local";
const OUTBOUND_AUDIENCE: &str = "outbound-local";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-test.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(issuer: &TestIssuer, whatsapp: &FakeWhatsAppApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            TRIO_AUDIENCE.to_string(),
        ),
        (
            "TRITON_OUTBOUND_AUDIENCE".to_string(),
            OUTBOUND_AUDIENCE.to_string(),
        ),
    ])
}

fn token_with_aud(issuer: &TestIssuer, aud: &str) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "carl-agent",
        "aud": aud,
        "exp": now() + 60,
        "iat": now(),
        "tenant": "acme",
        "scope": "outbound:send",
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_send_renders_and_couriers_to_platform() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    let token = token_with_aud(&issuer, OUTBOUND_AUDIENCE);
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "result": { "text": "Proactive hello from Carl" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        202,
        "expected 202 Accepted, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: Value = resp.json().await.expect("decode response");
    let trace_id = body["trace_id"]
        .as_str()
        .expect("response carries trace_id")
        .to_string();

    // The courier actually POSTed to the platform with the resolved
    // bearer + the rendered text body.
    let captured = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "exactly one outbound send");
    assert_eq!(captured[0].phone_number_id, PHONE_NUMBER_ID);
    assert_eq!(captured[0].authorization, format!("Bearer {ACCESS_TOKEN}"));
    assert_eq!(captured[0].body["type"], "text");
    assert_eq!(captured[0].body["to"], KNOWN_WA_ID);
    assert_eq!(
        captured[0].body["text"]["body"],
        "Proactive hello from Carl"
    );

    // The audit post record shares the response trace_id (FR-AU-1
    // two-record model; ADR-6 single audit pivot).
    let post = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(post["status_label"], "posted");
    assert_eq!(post["trace_id"], trace_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trio_audience_token_is_rejected_on_outbound() {
    // A token minted for the HTTP-trio audience must NOT be accepted on
    // the outbound surface — the dedicated audience is the per-surface
    // authorisation boundary.
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    let token = token_with_aud(&issuer, TRIO_AUDIENCE);
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "result": { "text": "should not ship" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(resp.status(), 401);

    // Belt-and-braces: nothing was couriered to the platform.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        whatsapp.captured().is_empty(),
        "rejected outbound must not post to the platform"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_adapter_is_not_found() {
    let issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &whatsapp)).await;

    let token = token_with_aud(&issuer, OUTBOUND_AUDIENCE);
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "telegram",
            "to": "42",
            "result": { "text": "no such adapter configured" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(resp.status(), 404);
}

// ---- local copies of the poll helpers used across the chat tests ----

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
                "audit line not found within {deadline:?}; stdout:\n{}",
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
            panic!("probe timed out after {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
