//! Issue #250 hardening — the resolved principal must not reach a signed
//! upstream token unvalidated, and the audit line must agree with what
//! was actually minted.
//!
//! Two crew reviews converged on the same structural problem: the
//! `identity.kind: upstream` strategy (FR-I-7) lets an out-of-process
//! resolver decide `sub`, `scopes` and `tenant` for every sender, and
//! those values are signed into the RS256 bearer Triton mints for the
//! upstream — but nothing validates them, and the one existing test of
//! the mode (`telegram_upstream_identity.rs`) asserts the tenant off the
//! **audit line** while running with no signing key at all, so
//! `bearer()` takes its static-token arm and no claim is ever produced.
//! The mode's security property has therefore never been demonstrated.
//!
//! Everything here decodes the bearer the FakeAgent actually received.
//! An audit-line assertion is inadmissible as proof: the audit pivot and
//! the token minter are different code paths, and the whole class of bug
//! below is them disagreeing.
//!
//! Three properties:
//!
//!   * a hostile (over-cap) tenant must FAIL CLOSED. Today it is
//!     silently replaced with `String::new()`
//!     (`static_upstream.rs:279-287`) and — because `signer.rs:128`
//!     omits an empty tenant claim — the resulting token is
//!     byte-identical to the supported `forward_principal = false`
//!     token that `forward_principal.rs` pins as correct. So a hostile
//!     resolver reply is indistinguishable from a legitimate
//!     configuration, downstream and in the audit log alike.
//!   * a hostile `sub` must fail closed too. `sub` is signed OUTSIDE
//!     the `forward_principal` gate (`static_upstream.rs:315`), so it
//!     reaches every minted token in every deployment, capped by
//!     nothing.
//!   * on every accepted request the audit line's tenant must equal the
//!     minted claim. This is the assertion that makes the other two
//!     provable rather than merely tested.
//!
//! No mocks per CLAUDE.md §1: real binary, real HMAC-signed inbound,
//! real resolver and command agents over TCP, real RSA signing.

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

/// Same throwaway RSA-2048 signer the #110 forwarding tests use. Test
/// only; never a real credential.
const SIGNING_KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_key.pem"
));
const JWKS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_jwks.json"
));

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-cloud-upstream-identity.yaml")
        .display()
        .to_string()
}

fn env_for(
    whatsapp: &FakeWhatsAppApi,
    agent: &FakeAgent,
    resolver: &FakeAgent,
) -> HashMap<String, String> {
    HashMap::from([
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
        // The mode under test only carries a resolved tenant when
        // forwarding is on. Off, every tenant collapses into the
        // deployment-static one — see `forwarding_off_is_refused`.
        (
            "TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL".to_string(),
            "true".to_string(),
        ),
    ])
}

fn inbound_envelope(wa_id: &str, text: &str) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "waba-1", "changes": [{ "field": "messages", "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "1555", "phone_number_id": "pn-1" },
            "contacts": [{ "profile": { "name": "Ada" }, "wa_id": wa_id }],
            "messages": [{
                "from": wa_id, "id": "wamid.1", "timestamp": "1717171717",
                "text": { "body": text }, "type": "text"
            }]
        }}]}]
    })
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn post_inbound(proc: &TritonProcess, text: &str) -> reqwest::Response {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let body = serde_json::to_vec(&inbound_envelope(UNKNOWN_WA_ID, text)).unwrap();
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

fn wait_for_audit(proc: &TritonProcess, deadline: Duration, m: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if m(&v) {
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

/// T8 — the assertion that makes the rest provable: what the audit line
/// claims and what the upstream actually received must be the same
/// tenant. The two are produced by different code paths (the dispatcher
/// audit pivot and `static_upstream::bearer`), and every finding in this
/// file is a case of them disagreeing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_tenant_equals_the_minted_claim() {
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-ada", "scopes": ["chat"], "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver),
    )
    .await;

    assert!(post_inbound(&proc, "hi").await.status().is_success());

    let bearer = wait_for(Duration::from_secs(5), || {
        agent.bearers_seen().into_iter().next()
    });
    let claims = jwt_claims(bearer.trim_start_matches("Bearer "));
    assert_eq!(
        claims["tenant"], "globex",
        "the resolver's tenant must reach the MINTED CLAIM, not just the \
         audit line; got: {claims}"
    );

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "assistant"
    });
    assert_eq!(
        dispatch["tenant"], claims["tenant"],
        "audit tenant and minted tenant must agree; audit={dispatch} claims={claims}"
    );
}

/// A hostile resolver reply must FAIL CLOSED. Today an over-cap tenant is
/// replaced with `String::new()` and the request proceeds; because
/// `signer.rs:128` omits an empty tenant claim, the upstream then
/// receives a token indistinguishable from a legitimate
/// `forward_principal = false` deployment. Nothing downstream, and no
/// audit line, can tell the two apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_cap_tenant_is_refused_not_blanked() {
    let hostile = "t".repeat(5000);
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-ada", "scopes": ["chat"], "tenant": hostile
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver),
    )
    .await;

    let _ = post_inbound(&proc, "hi").await;

    // The refusal is audited...
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:"))
            && v["tool"] == "assistant"
    });
    // ...and — the property that matters — the upstream was never called,
    // so no token carrying a blanked tenant was ever minted or sent.
    assert_eq!(
        agent.hits(),
        0,
        "a hostile tenant must never reach the upstream; got {} call(s) \
         with bearer(s): {:?}",
        agent.hits(),
        agent.bearers_seen()
    );
}

/// Found by live verification of the fail-closed path: the refusal is
/// correct, but the audit line it emits echoes the attacker-controlled
/// tenant **in full**. A hostile resolver can therefore write unbounded
/// data into the audit log and the 1024-entry ring buffer on every
/// request — the same class of harm #249 addressed, arriving through a
/// different door. Rejected values must be truncated before they are
/// recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_field_is_truncated_in_the_audit_line() {
    let hostile = "t".repeat(5000);
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-ada", "scopes": ["chat"], "tenant": hostile
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&whatsapp, &agent, &resolver))
            .await;

    let _ = post_inbound(&proc, "hi").await;

    let line = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["result"].as_str().is_some_and(|r| r.starts_with("error:"))
            && v["tool"] == "assistant"
    });
    let recorded = line["tenant"].as_str().unwrap_or_default();
    assert!(
        recorded.len() <= 160,
        "a rejected attacker-controlled field must be truncated before it \
         reaches the audit log; got {} bytes",
        recorded.len()
    );
    // Whatever survives must still be diagnosable.
    assert!(
        !recorded.is_empty(),
        "truncation must not erase the field entirely — an operator needs \
         to see what was refused"
    );
}

/// `sub` is signed OUTSIDE the `forward_principal` gate
/// (`static_upstream.rs:315`), so a resolver-supplied subject reaches
/// every minted token in every deployment with no cap and no charset
/// validation. An oversized one becomes an oversized `Authorization`
/// header on every upstream call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_cap_sub_is_refused() {
    let hostile = "s".repeat(100_000);
    let resolver = FakeAgent::start_returning(json!({
        "sub": hostile, "scopes": ["chat"], "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&whatsapp, &agent, &resolver),
    )
    .await;

    let _ = post_inbound(&proc, "hi").await;

    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:"))
            && v["tool"] == "assistant"
    });
    assert_eq!(
        agent.hits(),
        0,
        "an over-cap sub must never be signed into an upstream bearer"
    );
}
