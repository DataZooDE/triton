//! #100 — dedicated issuer for the outbound surface (mirror-image
//! static-upstream signing).
//!
//! On substrates without an external OIDC issuer, the registered
//! upstream agent holds its own signing key, serves JWKS on its
//! internal FQDN, and signs short-TTL tokens with the outbound
//! audience — the exact inverse of Triton's static-upstream signing
//! (#83). For that to work, `/v1/outbound` must be able to trust a
//! DISTINCT issuer from the HTTP trio's:
//!
//! - `TRITON_OUTBOUND_ISSUER` — per-surface issuer, falling back to
//!   `TRITON_OIDC_ISSUER` when unset (existing behaviour, covered by
//!   `outbound.rs`).
//! - `TRITON_OUTBOUND_JWKS_URL` — explicit JWKS document URL so the
//!   agent does not need to serve an OIDC discovery endpoint.
//!
//! No mocks: real binary, two real issuers (Ed25519 JWKS), real HTTP
//! to the in-repo `FakeWhatsAppApi`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::{TestIssuer, TritonProcess};

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
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

fn env_for(
    trio_issuer: &TestIssuer,
    whatsapp: &FakeWhatsAppApi,
    extra: &[(&str, String)],
) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        ("TRITON_OIDC_ISSUER".to_string(), trio_issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            TRIO_AUDIENCE.to_string(),
        ),
        (
            "TRITON_OUTBOUND_AUDIENCE".to_string(),
            OUTBOUND_AUDIENCE.to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k.to_string(), v.clone());
    }
    env
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

async fn post_outbound(proc: &TritonProcess, token: &str, text: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(token)
        .json(&json!({
            "adapter": "whatsapp",
            "to": KNOWN_WA_ID,
            "result": { "text": text },
        }))
        .send()
        .await
        .expect("POST /v1/outbound")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_verifies_against_dedicated_issuer() {
    // The agent's own issuer is DISTINCT from the trio's. A token the
    // agent signs itself (outbound audience) must be accepted on
    // /v1/outbound, even though the trio knows nothing about that key.
    let trio_issuer = TestIssuer::start().await;
    let agent_issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(
            &trio_issuer,
            &whatsapp,
            &[("TRITON_OUTBOUND_ISSUER", agent_issuer.issuer_url())],
        ),
    )
    .await;

    open_service_window(&proc, KNOWN_WA_ID).await;
    let _reply = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });

    let resp = post_outbound(&proc, &outbound_token(&agent_issuer), "Agent-signed send").await;
    assert_eq!(
        resp.status(),
        202,
        "agent-issuer token must be accepted on /v1/outbound, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );

    // The send actually reached the platform.
    let sent = wait_for(Duration::from_secs(2), || {
        whatsapp
            .captured()
            .into_iter()
            .find(|m| m.body["text"]["body"] == "Agent-signed send")
    });
    assert_eq!(sent.body["to"], KNOWN_WA_ID);
    assert_eq!(sent.phone_number_id, PHONE_NUMBER_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trio_issuer_token_is_rejected_when_dedicated_issuer_is_set() {
    // Once the outbound surface trusts the agent's issuer, a token
    // signed by the TRIO's issuer must no longer pass — even with the
    // correct outbound audience. Per-surface issuer separation, the
    // same boundary the per-surface audience already draws.
    let trio_issuer = TestIssuer::start().await;
    let agent_issuer = TestIssuer::start().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(
            &trio_issuer,
            &whatsapp,
            &[("TRITON_OUTBOUND_ISSUER", agent_issuer.issuer_url())],
        ),
    )
    .await;

    let resp = post_outbound(&proc, &outbound_token(&trio_issuer), "should not ship").await;
    assert_eq!(resp.status(), 401);

    // Belt-and-braces: nothing was couriered to the platform.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        whatsapp.captured().is_empty(),
        "rejected outbound must not post to the platform"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_jwks_url_works_without_discovery() {
    // The mirror-image agent serves only a JWKS document — no
    // /.well-known/openid-configuration. With TRITON_OUTBOUND_JWKS_URL
    // set, the outbound verifier must fetch keys from that URL
    // directly instead of attempting discovery.
    let trio_issuer = TestIssuer::start().await;
    let agent_issuer = TestIssuer::start_jwks_only().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(
            &trio_issuer,
            &whatsapp,
            &[
                ("TRITON_OUTBOUND_ISSUER", agent_issuer.issuer_url()),
                ("TRITON_OUTBOUND_JWKS_URL", agent_issuer.jwks_url()),
            ],
        ),
    )
    .await;

    open_service_window(&proc, KNOWN_WA_ID).await;
    let _reply = wait_for(Duration::from_secs(2), || {
        let v = whatsapp.captured();
        (!v.is_empty()).then_some(v)
    });

    let resp = post_outbound(&proc, &outbound_token(&agent_issuer), "JWKS-only send").await;
    assert_eq!(
        resp.status(),
        202,
        "JWKS-URL-configured verifier must accept the agent token, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let sent = wait_for(Duration::from_secs(2), || {
        whatsapp
            .captured()
            .into_iter()
            .find(|m| m.body["text"]["body"] == "JWKS-only send")
    });
    assert_eq!(sent.body["to"], KNOWN_WA_ID);
}

/// Post a signed inbound webhook so the recipient's 24-hour service
/// window opens (#94), allowing a subsequent free-form proactive send.
async fn open_service_window(proc: &TritonProcess, wa_id: &str) {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let envelope = json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": PHONE_NUMBER_ID },
            "messages": [{ "from": wa_id, "id": "wamid.X", "timestamp": "1700000000",
                "type": "text", "text": { "body": "hi" } }]
        }, "field": "messages" }] }]
    });
    let body = serde_json::to_vec(&envelope).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(APP_SECRET.as_bytes()).expect("hmac key");
    mac.update(&body);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook");
    assert!(resp.status().is_success());
}

// ---- local copy of the poll helper used across the chat tests ----

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
