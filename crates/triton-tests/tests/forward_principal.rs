//! #110 — opt-in forwarding of the resolved sender's `scope` + `tenant`
//! on the static-upstream **signed** bearer.
//!
//! By default the minted RS256 token carries only `sub` (+ a
//! deployment-static `tenant`). With `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true`
//! the token instead carries the resolved sender's `scope`
//! (space-delimited) and `tenant`, so an agent can act on them without a
//! second lookup. This proves the carriage end to end: a WhatsApp
//! inbound from an unknown sender is resolved (`identity.kind: upstream`)
//! to `{sub, scopes, tenant}`, then dispatched to the command agent with
//! a signed bearer we decode and assert.
//!
//! No mocks: real `triton`, real `FakeAgent`s (resolver + command) over
//! `TRITON_STATIC_UPSTREAMS`, real HMAC inbound, Triton signing with a
//! throwaway RSA key (below). The agent side captures the bearer; we
//! base64-decode its JWT payload (no verification needed — we're
//! asserting what Triton minted).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::upstream_fixture::FakeAgent;

const APP_SECRET: &str = "whatsapp-app-secret-for-test";
const UNKNOWN_WA_ID: &str = "490000000001";
const KID: &str = "triton-test-signer";

/// Throwaway RSA-2048 key + matching JWKS, for Triton's static-upstream
/// signer only. Test-only; never a real credential.
const SIGNING_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC3ja9HSrWiJLEK
Oj7YmUHGWA+jrGmtQEZxHOXkhOIJOIMcpYQYrjbCvUvdg/i9Z4j4TmOohuWKhmBI
6MN+XSXmwRUOJ2uqZQCstFIp6GrdbbmbRrnRN2BhahtLbPC23/dsqR3oEqUfVfrp
uR0z2veUC7zjmLsOGY7iTaczZLNX2eMjKfMwr75hJGqtEs8/AyUAPRC+sUi1lMph
ZMYZmdaSXwG3q7+eV23ZQIXGkUzyzhkAv3HtXL0PFSxsvGP7DUeTeWYUoe3Bk1Dy
mzB+ziWHjSU2ReyzQCTHl1l7xE1U1UjFpn0mqVnbvjgaXAKiH5jbKN+QaT5WzmPH
aHHeo2uxAgMBAAECggEAAhZXICsqEhgzOC/N36YsgI4nTV/sSrdQpcAjoBvfuyWc
nhKGxEYU4tWGu5Pg2/yFqvcvPG8eRJs/FI0rDfCOugdHj0Pk/kjMP2qEhav6LR7u
jaS5/7ZOvwTXHx4zxYyZ8m8g4y71GDxg0FAV1C1hA9q3UOo/dEtXm9ywsk2qmWzg
sHKcURK0pYXFZI8iR3OnXJ/qQrDAd4eEoOGV1G4rKN0EYk8/v2tsMvX1fPTaYe//
1Q6VwjiFTPwBeaxJoeRAk+qe8KALVxHKlpDboagAWhBKGQaJB7iKFugRpjzF1e5h
J8RFa6M2b4SSgSOKiUaxcq5p2xtbIyHtFEckrZgRAQKBgQDhzw+KSQiXKSFGUaIT
Al2XdRGLY9tRPuanpCu6XEA+DilkO/XLbeP80qNZ30VqZFKpC3WC9e6sgtvi9Ntl
ZvUH2QdxxMWgvEaVwcRB/D2kTTxECXP/+2wazVyIttQTHl48LJjhEhXYHBc/B5iQ
ZlooCTM7W1bg8jaOVWKfP23S8QKBgQDQGFDD3yS3VAlasJ8UJU+nYgaMGlZ4hr6R
5hzkOxMlQhvwVBNeXIiGyvX8cUUR8mXgGC7yCfgl1/K1srNJ6sjSsTAixdx8CtYq
n2HQMMODWja3rI9Dy6RbPGL8ohlWZXOrl55Vm0WXT35uZRqjKQ6d8uXpwshBOqnL
ldfOwwWkwQKBgECPA4Fkygj1oGbLVgwbRAjWVpLElOKQmj9Zp4rbbx6Oy/S1U9u6
alFRI5TBScZWMm/UL9+mUnuN2jH0EXnXSrzYptE3Ec2XppKQWH0JEdKUpmNJVJne
FxU+m3MW2mEw8H5Bvd+zXP1xYpAJquu155bEspoIzjj35vMgpFalOs/xAoGAdLSe
Xyu/eL29vUn+/ZprUNGOIHcI9fGD4Wlv3KQw+Z1Y8/EDJ9G3k/kx+hFAjm8mqYaG
laH3tKmm6jY9jQAK/vb2qxnSrRKayC64+bzPedRXia1Sb9A+7hgw38S9dxHQzHRt
DU/WuKSRoLI9PTJiizzVqsNd8g9HePEhpkkD2kECgYEAo91wH9+VhweqbEeI+Vq4
zUgrd95Qha9XZ6acdmumGs/FseKIzrIv64rDIEimjC3WsFMQAEmxcG7zxMbC8I3y
g/rEoFAtCAkX9MfqQVGVMsZkYH9mLyDZkKMtXbg4ablmzuclz0NCLO8nwgQCiBNm
cUFPn8tKw7AvtWpo3gsHw30=
-----END PRIVATE KEY-----";

const JWKS: &str = r#"{"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":"triton-test-signer","n":"t42vR0q1oiSxCjo-2JlBxlgPo6xprUBGcRzl5ITiCTiDHKWEGK42wr1L3YP4vWeI-E5jqIblioZgSOjDfl0l5sEVDidrqmUArLRSKehq3W25m0a50TdgYWobS2zwtt_3bKkd6BKlH1X66bkdM9r3lAu845i7DhmO4k2nM2SzV9njIynzMK--YSRqrRLPPwMlAD0QvrFItZTKYWTGGZnWkl8Bt6u_nldt2UCFxpFM8s4ZAL9x7Vy9DxUsbLxj-w1Hk3lmFKHtwZNQ8pswfs4lh40lNkXss0Akx5dZe8RNVNVIxaZ9JqlZ2744GlwCoh-Y2yjfkGk-Vs5jx2hx3qNrsQ","e":"AQAB"}]}"#;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-cloud-upstream-identity.yaml")
        .display()
        .to_string()
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn inbound_envelope(wa_id: &str, text: &str) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": "100200300" },
            // The message id is derived from the body so a test can post
            // two DISTINCT inbounds (needed to prove a warn is rate-limited
            // rather than emitted per call) without them looking like a
            // redelivery of the same message.
            "messages": [{ "from": wa_id, "id": format!("wamid.{text}"), "timestamp": "1700000000",
                "type": "text", "text": { "body": text } }]
        }, "field": "messages" }] }]
    })
}

/// Common env: WhatsApp Cloud + `upstream` identity, static-upstream
/// SIGNED mode (RSA key + JWKS + issuer). `forward` toggles the #110 flag.
fn env_for(
    whatsapp: &FakeWhatsAppApi,
    agent: &FakeAgent,
    resolver: &FakeAgent,
    forward: bool,
) -> HashMap<String, String> {
    env_for_full(whatsapp, agent, resolver, forward, None)
}

fn env_for_full(
    whatsapp: &FakeWhatsAppApi,
    agent: &FakeAgent,
    resolver: &FakeAgent,
    forward: bool,
    scope_allowlist: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "assistant={},resolve_identity={}",
                agent.host_port(),
                resolver.host_port()
            ),
        ),
        // Static-upstream signing (Triton mints the agent bearer).
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), JWKS.to_string()),
        ("TRITON_JWT_KID".to_string(), KID.to_string()),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
    ]);
    if forward {
        env.insert(
            "TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL".to_string(),
            "true".to_string(),
        );
    }
    if let Some(allow) = scope_allowlist {
        env.insert(
            "TRITON_STATIC_UPSTREAM_SCOPE_ALLOWLIST".to_string(),
            allow.to_string(),
        );
    }
    env
}

async fn post_inbound(proc: &TritonProcess, wa_id: &str, text: &str) -> reqwest::Response {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let body = serde_json::to_vec(&inbound_envelope(wa_id, text)).unwrap();
    let sig = sign(&body, APP_SECRET);
    reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook")
}

/// Decode (without verifying) the JWT payload Triton minted.
fn jwt_claims(jwt: &str) -> Value {
    let payload = jwt.split('.').nth(1).expect("jwt has a payload segment");
    let bytes = URL_SAFE_NO_PAD.decode(payload).expect("base64url payload");
    serde_json::from_slice(&bytes).expect("payload json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_carries_resolved_scope_and_tenant() {
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat", "reports"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, true),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    // The command agent's bearer carries the resolved principal.
    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    assert_eq!(claims["sub"], "resolved-bob", "sub = resolved sender");
    // #114: resolved scopes ride a NON-authoritative private claim (array),
    // never the standard OAuth `scope`, so a downstream can't mistake
    // resolver-derived data for granted authorization.
    assert_eq!(
        claims["triton_sender_scopes"],
        json!(["chat", "reports"]),
        "resolved scopes under the private claim"
    );
    assert!(
        claims.get("scope").is_none(),
        "must NOT emit a standard OAuth `scope` claim: {claims}"
    );
    assert_eq!(claims["tenant"], "globex", "resolved sender tenant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_off_is_sub_only_by_default() {
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat", "reports"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, false),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    // Default contract unchanged: sub is the only per-sender claim; no
    // forwarded scopes, and no per-sender `tenant` (deployment tenant unset).
    assert_eq!(claims["sub"], "resolved-bob");
    assert!(
        claims.get("triton_sender_scopes").is_none() && claims.get("scope").is_none(),
        "no forwarded scopes by default: {claims}"
    );
    assert!(
        claims.get("tenant").is_none(),
        "no per-sender tenant by default: {claims}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_caps_and_sanitises_resolver_scopes() {
    // #114: a buggy/hostile resolver returning junk scopes (whitespace,
    // over-length) must not bloat or corrupt the token — they're dropped,
    // the clean ones survive, and the send still succeeds.
    let long = "x".repeat(200);
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat", "has space", long, "reports"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, true),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    // The whitespace + over-length scopes are dropped; clean ones kept.
    assert_eq!(
        claims["triton_sender_scopes"],
        json!(["chat", "reports"]),
        "junk scopes sanitised away: {claims}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_applies_scope_allowlist() {
    // #114: with an operator allowlist, only blessed scopes propagate.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat", "admin"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for_full(&whatsapp, &agent, &resolver, true, Some("chat")),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    assert_eq!(
        claims["triton_sender_scopes"],
        json!(["chat"]),
        "only allowlisted scopes forwarded (`admin` dropped): {claims}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_carries_resolved_groups() {
    // RBAC: the resolved sender's `groups` ride a NON-authoritative private
    // claim `triton_sender_groups` (array) — NEVER `roles` (which a
    // downstream like escurel derives admin from). A downstream sets its
    // `groups_claim` to `triton_sender_groups` to consume them.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat"],
        "groups": ["team-acme", "moderator"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, true),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    assert_eq!(
        claims["triton_sender_groups"],
        json!(["team-acme", "moderator"]),
        "resolved groups under the private claim: {claims}"
    );
    // Must NOT emit a `roles` claim — that would let escurel derive admin
    // from a forwarded value.
    assert!(
        claims.get("roles").is_none(),
        "must never emit `roles` (admin-derivation claim): {claims}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_off_omits_groups_by_default() {
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "groups": ["team-acme"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, false),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    assert!(
        claims.get("triton_sender_groups").is_none(),
        "no forwarded groups by default: {claims}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_applies_group_allowlist() {
    // An operator allowlist blesses which groups may propagate.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "groups": ["team-acme", "secret-cabal"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let mut env = env_for(&whatsapp, &agent, &resolver, true);
    env.insert(
        "TRITON_STATIC_UPSTREAM_GROUP_ALLOWLIST".to_string(),
        "team-acme".to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(&bearer);
    assert_eq!(
        claims["triton_sender_groups"],
        json!(["team-acme"]),
        "only allowlisted groups forwarded (`secret-cabal` dropped): {claims}"
    );
}

// ---------------------------------------------------------------------------
// Observability of the forwarding decision (DX, not security).
//
// The failure these pin: with forwarding OFF, a caller authenticates fine and
// a group-filtering upstream returns NOTHING. That is indistinguishable from a
// broken scope filter in the upstream, and it cost a consuming project a full
// debugging cycle. Triton must SAY that it made the decision (once, at
// startup) and SAY when the data actually disappears (at the mint).
// ---------------------------------------------------------------------------

/// Substring identifying the once-at-startup forwarding-decision line.
const DECISION_LOG: &str = "static-upstream principal forwarding decision";
/// Substring identifying the at-the-mint "groups were dropped" warning.
const DROPPED_LOG: &str = "principal forwarding is OFF";

/// Triton logs JSON on stdout; stderr is folded in so a panic/backtrace
/// shows up in the failure message too.
fn log_lines(proc: &TritonProcess) -> Vec<String> {
    let mut lines = proc.stdout_snapshot();
    lines.extend(proc.stderr_snapshot());
    lines
}

fn log_hits(proc: &TritonProcess, needle: &str) -> usize {
    log_lines(proc)
        .iter()
        .filter(|l| l.contains(needle))
        .count()
}

/// Block until the command agent has been dispatched to `n` times.
fn wait_for_bearers(agent: &FakeAgent, n: usize) -> Vec<String> {
    wait_for(Duration::from_secs(5), || {
        let seen = agent.bearers_seen();
        (seen.len() >= n).then_some(seen)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_logs_forwarding_off_exactly_once() {
    // The decision line must be a CONFIG line, not a per-request one: two
    // dispatches, still one line. (Its `forward_principal=true` counterpart
    // is the positive control in `startup_logs_forwarding_on_with_allowlist_size`.)
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "groups": ["team-acme"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, false),
    )
    .await;

    for text in ["one", "two"] {
        let resp = post_inbound(&proc, UNKNOWN_WA_ID, text).await;
        assert!(resp.status().is_success(), "{}", resp.status());
    }
    wait_for_bearers(&agent, 2);

    let logs = log_lines(&proc).join("\n");
    assert_eq!(
        log_hits(&proc, DECISION_LOG),
        1,
        "the forwarding decision is logged once at config time, not per call: {logs}"
    );
    assert!(
        logs.contains("\"forward_principal\":false"),
        "the decision line must name the OFF state: {logs}"
    );
    assert!(
        logs.contains("\"group_allowlist_size\":0"),
        "the decision line must name the group allowlist size: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_logs_forwarding_on_with_allowlist_size() {
    // Positive control for the line above: the SAME fields must report the
    // ON state and the real allowlist size, so an operator reading the line
    // can tell a configured allowlist from an absent one.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let mut env = env_for(&whatsapp, &agent, &resolver, true);
    env.insert(
        "TRITON_STATIC_UPSTREAM_GROUP_ALLOWLIST".to_string(),
        "team-acme,ops".to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());
    wait_for_bearers(&agent, 1);

    let logs = log_lines(&proc).join("\n");
    assert_eq!(log_hits(&proc, DECISION_LOG), 1, "{logs}");
    assert!(
        logs.contains("\"forward_principal\":true"),
        "the decision line must name the ON state: {logs}"
    );
    assert!(
        logs.contains("\"group_allowlist_size\":2"),
        "the decision line must name the configured allowlist size: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_off_warns_once_at_the_instant_groups_are_dropped() {
    // The precise instant the information disappears: a resolved sender WITH
    // memberships is minted into a token that will carry none. Warn there —
    // and only once, so a busy gateway can't flood the log.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "groups": ["team-acme", "moderator"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, false),
    )
    .await;

    for text in ["one", "two"] {
        let resp = post_inbound(&proc, UNKNOWN_WA_ID, text).await;
        assert!(resp.status().is_success(), "{}", resp.status());
    }
    let bearers = wait_for_bearers(&agent, 2);

    // The loss is real: the token genuinely carries no groups.
    let claims = jwt_claims(&bearers[0]);
    assert!(
        claims.get("triton_sender_groups").is_none(),
        "precondition: forwarding off drops the groups: {claims}"
    );
    let logs = log_lines(&proc).join("\n");
    assert_eq!(
        log_hits(&proc, DROPPED_LOG),
        1,
        "one warn for two dispatches (rate-limited, not per call): {logs}"
    );
    assert!(
        logs.contains("\"dropped_groups\":2"),
        "the warn must say how much was dropped: {logs}"
    );
    // Never log the membership VALUES — they are sender attributes, and the
    // count is all an operator needs to recognise the failure.
    assert!(
        !logs.contains("team-acme"),
        "the warn must not print membership values: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_on_does_not_warn_about_dropped_groups() {
    // Nothing is lost when forwarding is on, so the warn must stay silent.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "groups": ["team-acme"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, true),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let bearers = wait_for_bearers(&agent, 1);

    // Positive control: the groups really did ride the token, and the log
    // snapshot really is populated and searchable (the decision line is
    // there) — so the absence below is silence, not a broken probe.
    let claims = jwt_claims(&bearers[0]);
    assert_eq!(claims["triton_sender_groups"], json!(["team-acme"]));
    let logs = log_lines(&proc).join("\n");
    assert_eq!(log_hits(&proc, DECISION_LOG), 1, "{logs}");
    assert_eq!(
        log_hits(&proc, DROPPED_LOG),
        0,
        "nothing was dropped, so nothing to warn about: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_off_is_silent_when_the_sender_has_no_groups() {
    // Forwarding off is the DEFAULT. A sender with no memberships loses
    // nothing, so the warn must not fire — otherwise every default
    // deployment logs a warning that means nothing.
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-bob",
        "scopes": ["chat"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver, false),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_WA_ID, "hi").await;
    assert!(resp.status().is_success(), "{}", resp.status());
    wait_for_bearers(&agent, 1);

    // Positive control, as above: the snapshot is populated and searchable.
    let logs = log_lines(&proc).join("\n");
    assert_eq!(log_hits(&proc, DECISION_LOG), 1, "{logs}");
    assert_eq!(
        log_hits(&proc, DROPPED_LOG),
        0,
        "no memberships means no loss, so no warn: {logs}"
    );
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
