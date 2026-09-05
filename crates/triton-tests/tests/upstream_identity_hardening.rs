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

/// The clamp must be an INVARIANT of the audit emitter, not a habit of
/// three call sites. A crew review found the first cut covered 3 of 5
/// `AuditRecord` sinks: `record_rejection` and `emit_stream_audit` still
/// wrote the raw principal, and `record_rejection` additionally emits
/// `error_detail`, into which the chat adapters interpolate the
/// **unvalidated** tenant (`tenant \`{tenant}\` rate limit hit …`). So a
/// hostile tenant still reached stdout and the ring buffer twice per
/// request, through a different door.
///
/// This asserts the property directly over every audit line the process
/// emits, so a future sink added without clamping fails here rather than
/// in production. Driven through the per-tenant rate limiter (burst 1),
/// which is the reachable path to `record_rejection`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_audit_line_carries_an_oversized_field() {
    let hostile = "t".repeat(5000);
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-ada", "scopes": ["chat"], "tenant": hostile
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let mut env = env_for(&whatsapp, &agent, &resolver);
    env.insert(
        "TRITON_MANIFEST_PATH".to_string(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/manifest-whatsapp-upstream-tightlimit.yaml")
            .display()
            .to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // Two inbounds: the second trips the per-tenant limiter and reaches
    // `record_rejection`, the sink the first cut missed.
    for _ in 0..2 {
        let _ = post_inbound(&proc, "hi").await;
    }
    // Wait until a refusal has been recorded, whichever gate fired.
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:"))
    });

    // Now hold the invariant over EVERY audit line, every string field.
    const CEILING: usize = 512;
    for line in proc.stdout_snapshot() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["kind"] != "audit" {
            continue;
        }
        for (field, val) in v.as_object().expect("audit object") {
            if let Some(text) = val.as_str() {
                assert!(
                    text.len() <= CEILING,
                    "audit field `{field}` is {} bytes — an attacker-controlled \
                     value reached an audit sink unclamped. Line: {}",
                    text.len(),
                    &line[..line.len().min(300)]
                );
            }
        }
    }
}

/// A hostile resolver reply must be refused AT THE RESOLVER BOUNDARY,
/// before the value is used for anything.
///
/// `PerTenantBuckets::try_take` does `buckets.entry(tenant.to_string())
/// .or_insert_with(...)` — an unbounded insert keyed on the tenant, run
/// BEFORE the mint-time validation added earlier in this branch. Its own
/// doc-comment asserts "the cardinality is bounded by the manifest, not
/// by inbound traffic"; under FR-I-7 `upstream` that is false, and a
/// resolver returning a fresh oversized tenant per message grows process
/// memory without bound.
///
/// The observable: today the second hostile inbound is refused with
/// `error:ratelimit`, which only happens if the hostile tenant was used
/// as a limiter key. After the fix both are refused with `error:auth` at
/// resolution and the limiter never sees the value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_tenant_never_reaches_the_rate_limiter() {
    let hostile = "t".repeat(5000);
    let resolver = FakeAgent::start_returning(json!({
        "sub": "resolved-ada", "scopes": ["chat"], "tenant": hostile
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let mut env = env_for(&whatsapp, &agent, &resolver);
    env.insert(
        "TRITON_MANIFEST_PATH".to_string(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/manifest-whatsapp-upstream-tightlimit.yaml")
            .display()
            .to_string(),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    for _ in 0..3 {
        let _ = post_inbound(&proc, "hi").await;
    }
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:"))
    });
    std::thread::sleep(Duration::from_millis(300));

    let ratelimited: Vec<Value> = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["result"]
                    .as_str()
                    .is_some_and(|r| r.contains("ratelimit"))
        })
        .collect();
    assert!(
        ratelimited.is_empty(),
        "a hostile tenant reached the per-tenant limiter and became a map \
         key before validation ran; got {} ratelimit line(s)",
        ratelimited.len()
    );
    assert_eq!(
        agent.hits(),
        0,
        "and it must never reach the upstream either"
    );
}

/// On a boundary that cannot be made cryptographic, detection is the
/// only compensating control — and it is missing.
///
/// Under `identity.kind: upstream` the resolver REPLACES the asserted
/// sender id with whatever principal it chooses, and the asserted id is
/// then dropped: `AuditRecord` records `who`/`subject` (the RESOLVED
/// sub) and nothing else. So a session driven by a spoofed platform id
/// produces audit lines byte-identical to the victim's — there is no
/// post-hoc way to answer "which sessions used this enrolment?" during
/// an incident, and FR-AU-2's accountability claim does not hold on this
/// path.
///
/// The audit line must therefore carry the RAW platform sender id
/// alongside the resolved subject.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_audit_line_records_the_raw_sender_not_only_the_resolved_one() {
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

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "assistant"
    });
    // The resolved identity is what authorises...
    assert_eq!(dispatch["subject"], "resolved-ada");
    // ...but the asserted one is what an incident responder needs.
    assert_eq!(
        dispatch["sender_ref"], UNKNOWN_WA_ID,
        "the audit line must name the RAW platform sender the resolver was \
         asked about, or an impersonation is indistinguishable from the \
         victim's own session; got: {dispatch}"
    );
}

/// `sender_ref` is raw platform input — a phone number on WhatsApp —
/// and it now lands on every audit line. Under `sender_table` and
/// `azure` the resolved subject is already derived from that same id, so
/// recording it a second time adds no forensic value and only extends
/// how much personal data sits in logs and the ring buffer.
///
/// It earns its place only where the resolver REPLACES the asserted
/// identity, i.e. `identity.kind: upstream`. Everywhere else it must be
/// omitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender_ref_is_recorded_only_where_the_identity_was_replaced() {
    // `sender_table`: subject is derived from the sender id, so no
    // second copy.
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let mut env = env_for(&whatsapp, &agent, &agent);
    env.insert(
        "TRITON_MANIFEST_PATH".to_string(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/manifest-whatsapp-test.yaml")
            .display()
            .to_string(),
    );
    env.insert(
        "TRITON_STATIC_UPSTREAMS".to_string(),
        format!("assistant={}", agent.host_port()),
    );
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    // The sender the `sender_table` fixture actually enumerates.
    let body = serde_json::to_vec(&inbound_envelope("491701234567", "hi")).unwrap();
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let sig = sign(&body, APP_SECRET);
    let _ = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound");

    let line = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch"
    });
    assert!(
        line.get("sender_ref").is_none(),
        "sender_table already derives the subject from the sender id; a \
         second copy is only extra personal data in the log. got: {line}"
    );
}

/// The residual risk of `identity.kind: upstream` is real and accepted —
/// the resolver maps an UNSIGNED sender id to a principal, so it is an
/// authorization table rather than an identity proof. An accepted risk
/// that lives only in a doc comment is one nobody decided; it should be
/// visible to whoever starts the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_upstream_trust_model_is_stated_at_boot() {
    let agent = FakeAgent::start_echoing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&whatsapp, &agent, &agent))
            .await;

    let logs = wait_for(Duration::from_secs(5), || {
        let joined = proc.stdout_snapshot().join("\n");
        joined.contains("authorization table").then_some(joined)
    });
    assert!(
        logs.contains("not a cryptographic identity proof"),
        "the boot warning must say what the mode does NOT give you"
    );
    assert!(
        logs.contains("resolve_identity"),
        "and name the resolver, so an operator knows which adapter it is about"
    );
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
        // Recorded against the ADAPTER, not the command tool: since the
        // resolver-boundary check landed, a hostile reply is refused
        // before any dispatch to `assistant` happens at all.
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:auth"))
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

/// The clamp must bite on a value that genuinely reaches an audit sink.
///
/// An earlier version of this test drove a hostile resolver TENANT, but
/// once resolver validation moved to the boundary that value stops short
/// of the audit path and the assertion became vacuous — it passed with
/// `clamp_audited` deleted (crew review, verified by mutation). The
/// honest path is `sender_ref`: the RAW platform sender id, recorded for
/// impersonation detection, which by construction is unvalidated
/// attacker-supplied input and is supposed to reach the line.
///
/// Unit tests for `clamp_audited` itself live in `triton-core::audit`;
/// this proves the emitter actually applies it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_platform_sender_is_clamped_in_the_audit_line() {
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

    // A 5000-digit `wa_id`: platform input, no validation between the
    // wire and the audit line.
    let huge_sender = "9".repeat(5000);
    let body = serde_json::to_vec(&inbound_envelope(&huge_sender, "hi")).unwrap();
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let sig = sign(&body, APP_SECRET);
    let _ = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound");

    let line = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["sender_ref"].is_string()
    });
    let recorded = line["sender_ref"].as_str().expect("sender_ref");
    assert!(
        recorded.ends_with("…[5000 bytes]"),
        "the clamp must truncate AND name the true length; got {} bytes: {}",
        recorded.len(),
        &recorded[..recorded.len().min(80)]
    );
    assert!(recorded.len() < 200, "got {} bytes", recorded.len());

    // The same bound must hold on the /v1/audit ring buffer, which is
    // what an operator actually tails.
    let audit: Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/audit"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/audit")
        .json()
        .await
        .expect("decode audit");
    for e in audit["entries"].as_array().expect("entries") {
        if let Some(sr) = e["sender_ref"].as_str() {
            assert!(
                sr.len() < 200,
                "ring buffer entry unclamped: {} bytes",
                sr.len()
            );
        }
    }
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
        // Recorded against the ADAPTER, not the command tool: since the
        // resolver-boundary check landed, a hostile reply is refused
        // before any dispatch to `assistant` happens at all.
        v["kind"] == "audit"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:auth"))
    });
    assert_eq!(
        agent.hits(),
        0,
        "an over-cap sub must never be signed into an upstream bearer"
    );
}
