//! #191 PR-T3 — Twilio-WhatsApp Content Template proactive sends.
//!
//! Twilio's WhatsApp channel requires pre-approved **Content Templates**
//! (Twilio Content API) for anything beyond a free-form reply inside an
//! active conversation — there is no ad-hoc "build an interactive button
//! set at send time" the way Meta's direct Graph API allows (verified
//! against Twilio's Message resource docs: `ContentSid` + `ContentVariables`
//! are the only levers `POST /Messages.json` exposes for rich content).
//! So Triton's existing `category` + `variables` proactive-send mechanism
//! (#94, reused verbatim from `OutboundRequest`) resolves a manifest
//! `templates` entry to a `ContentSid`, and `variables` become
//! `ContentVariables` (`{"1": v0, "2": v1, ...}`, matching Twilio's
//! `{{1}}`/`{{2}}` placeholder convention).
//!
//! No mocks: real binary, real OIDC issuer, real HTTP to `FakeTwilioApi`.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::chat_courier_fixture::FakeTwilioApi;
use triton_tests::{TestIssuer, TritonProcess};

const ACCOUNT_SID: &str = "ACtest00000000000000000000000000";
const KNOWN_WA_NUMBER: &str = "+491701234567";
const OUTBOUND_AUDIENCE: &str = "outbound-local";

fn manifest_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-twilio-whatsapp-template-test.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(port: u16, issuer: &TestIssuer, twilio: &FakeTwilioApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TWILIO_API_BASE".to_string(), twilio.url()),
        ("TRITON_CHAT_WEBHOOK_PORT".to_string(), port.to_string()),
        (
            "TRITON_TWILIO_WHATSAPP_PUBLIC_URL".to_string(),
            format!("http://127.0.0.1:{port}/twilio-whatsapp/webhook"),
        ),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "agents-local".to_string(),
        ),
        (
            "TRITON_OUTBOUND_AUDIENCE".to_string(),
            OUTBOUND_AUDIENCE.to_string(),
        ),
    ])
}

fn outbound_token(issuer: &TestIssuer) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "carl-agent",
        "aud": OUTBOUND_AUDIENCE,
        "exp": now() + 60,
        "iat": now(),
        "tenant": "acme",
        "scope": "outbound:send",
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn category_send_uses_content_template() {
    let port = triton_tests::free_tcp_port();
    let issuer = TestIssuer::start().await;
    let twilio = FakeTwilioApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(port, &issuer, &twilio))
            .await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer))
        .json(&json!({
            "adapter": "twilio-whatsapp",
            "to": KNOWN_WA_NUMBER,
            "category": "utility",
            "variables": ["Alice", "9am"],
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        202,
        "got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );

    let captured = wait_for(Duration::from_secs(2), || {
        let v = twilio.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1);
    let sent = &captured[0];
    assert_eq!(sent.account_sid_in_path, ACCOUNT_SID);
    assert_eq!(
        sent.form.get("ContentSid").map(String::as_str),
        Some("HXtest00000000000000000000000000"),
        "category `utility` must resolve to the manifest's ContentSid"
    );
    let vars_json = sent
        .form
        .get("ContentVariables")
        .expect("ContentVariables present");
    let vars: Value = serde_json::from_str(vars_json).expect("ContentVariables is JSON");
    assert_eq!(vars["1"], "Alice");
    assert_eq!(vars["2"], "9am");
    assert_eq!(
        sent.form.get("To").map(String::as_str),
        Some(format!("whatsapp:{KNOWN_WA_NUMBER}").as_str())
    );

    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(post_audit["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_category_is_rejected() {
    let port = triton_tests::free_tcp_port();
    let issuer = TestIssuer::start().await;
    let twilio = FakeTwilioApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(port, &issuer, &twilio))
            .await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer))
        .json(&json!({
            "adapter": "twilio-whatsapp",
            "to": KNOWN_WA_NUMBER,
            "category": "marketing",
            "variables": [],
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        400,
        "no `marketing` template configured on this adapter — must be refused"
    );

    std::thread::sleep(Duration::from_millis(250));
    assert!(twilio.captured().is_empty());
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
