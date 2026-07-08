//! Email outbound courier (`POST /v1/outbound`, `adapter: email`).
//!
//! A registered agent submits a proactive email; Triton resolves the email
//! adapter's courier, renders the surface through the email surface mapper
//! (the SAME one the `/v1/surface/render` preview uses), and POSTs
//! `{from, to, subject, html, text}` to the transactional-email API. Email
//! has no service window, so a rendered surface ships directly.
//!
//! Auth mirrors the WhatsApp outbound suite: a dedicated outbound audience +
//! the `outbound:send` scope + a sender_table tenant binding. No mocks: real
//! binary, real Ed25519 OIDC issuer, real HTTP to the in-repo `FakeEmailApi`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::chat_courier_fixture::FakeEmailApi;
use triton_tests::{TestIssuer, TritonProcess};

const API_KEY: &str = "email-api-key-for-test";
const FROM: &str = "assistant@datazoo.example";
const KNOWN_EMAIL: &str = "maria@company.example";
const TRIO_AUDIENCE: &str = "agents-local";
const OUTBOUND_AUDIENCE: &str = "outbound-local";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-email-test.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(issuer: &TestIssuer, email: &FakeEmailApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_EMAIL_API_BASE".to_string(), email.url()),
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

fn token_full(issuer: &TestIssuer, aud: &str, tenant: &str, scope: &str) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "carl-agent",
        "aud": aud,
        "exp": now() + 60,
        "iat": now(),
        "tenant": tenant,
        "scope": scope,
    }))
}

fn token_with_aud(issuer: &TestIssuer, aud: &str) -> String {
    token_full(issuer, aud, "acme", "outbound:send")
}

/// A surface carrying text + a button + a report — email renders all three.
fn rich_surface() -> Value {
    json!({
        "surface": { "components": [
            { "kind": "text", "value": "**Initech** renewal is at risk." },
            { "kind": "button", "label": "Open the account",
              "tool": "assistant", "args": { "question": "latest on initech-corp?" } },
            { "kind": "report", "report_id": "customer-briefing", "args": {} }
        ] }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_email_renders_complete_and_couriers_to_the_api() {
    let issuer = TestIssuer::start().await;
    let email = FakeEmailApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &email)).await;

    let token = token_with_aud(&issuer, OUTBOUND_AUDIENCE);
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "email",
            "to": KNOWN_EMAIL,
            "result": rich_surface(),
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

    // The courier POSTed the email to the transactional API with the resolved
    // API key, the derived subject, and an HTML body that renders the message
    // COMPLETE: the button as a link and the report caption inline.
    let sent = wait_for(Duration::from_secs(2), || {
        email.captured().into_iter().next()
    });
    assert_eq!(sent.bearer, format!("Bearer {API_KEY}"));
    assert_eq!(sent.body["from"], FROM);
    assert_eq!(sent.body["to"], KNOWN_EMAIL);
    assert_eq!(
        sent.body["subject"], "Initech renewal is at risk.",
        "subject derived from lead text (markdown stripped): {}",
        sent.body
    );
    let html = sent.body["html"].as_str().expect("html body");
    assert!(html.starts_with("<!doctype html>"), "full document: {html}");
    assert!(
        html.contains("<strong>Initech</strong>"),
        "bold renders: {html}"
    );
    assert!(
        html.contains(">Open the account</a>"),
        "button renders as a link, not deferred: {html}"
    );
    assert!(
        html.contains("customer-briefing"),
        "report caption renders inline: {html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_email_records_a_post_audit() {
    let issuer = TestIssuer::start().await;
    let email = FakeEmailApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &email)).await;

    let token = token_with_aud(&issuer, OUTBOUND_AUDIENCE);
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "email",
            "to": KNOWN_EMAIL,
            "result": { "surface": { "components": [ { "kind": "text", "value": "hi" } ] } },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    let body: Value = resp.json().await.expect("decode");
    let trace_id = body["trace_id"].as_str().expect("trace_id").to_string();

    // The send audits a `phase: post` record on the `email` protocol sharing
    // the response trace_id (ADR-6 single audit pivot).
    let post = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "post"
            && v["protocol"] == "email"
            && v["trace_id"] == trace_id.as_str()
    });
    assert_eq!(post["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_tenant_recipient_is_forbidden() {
    // #113: `maria@company.example` is bound to tenant `acme`; a token for
    // `globex` must not be allowed to email her, even with audience + scope.
    let issuer = TestIssuer::start().await;
    let email = FakeEmailApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &email)).await;

    let token = token_full(&issuer, OUTBOUND_AUDIENCE, "globex", "outbound:send");
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "email",
            "to": KNOWN_EMAIL,
            "result": { "surface": { "components": [ { "kind": "text", "value": "x" } ] } },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        403,
        "cross-tenant recipient must be forbidden"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        email.captured().is_empty(),
        "a forbidden outbound must not post to the API"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_outbound_scope_is_forbidden() {
    let issuer = TestIssuer::start().await;
    let email = FakeEmailApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &email)).await;

    let token = token_full(&issuer, OUTBOUND_AUDIENCE, "acme", "chat");
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(&token)
        .json(&json!({
            "adapter": "email",
            "to": KNOWN_EMAIL,
            "result": { "surface": { "components": [ { "kind": "text", "value": "x" } ] } },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        403,
        "missing outbound:send scope must be forbidden"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(email.captured().is_empty(), "no scope → nothing couriered");
}

// ---- poll helpers (local copies, as in outbound.rs) ----

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
