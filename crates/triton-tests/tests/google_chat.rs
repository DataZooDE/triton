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
