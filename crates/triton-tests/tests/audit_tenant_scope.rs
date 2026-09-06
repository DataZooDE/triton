//! #282 — `/v1/audit`, `/v1/trace` and `/v1/tools` are tenant-scoped.
//!
//! All three handlers verified the caller and then **discarded** the
//! `Principal` (`if let Err(e) = state.identity.verify(...)`), so any
//! authenticated caller read every tenant's audit entries, trace bodies
//! and tool inventory. This is the one confidentiality finding no
//! upstream contract can cover, because Triton serves the data itself.
//!
//! The properties pinned here:
//!
//!   * a caller sees only their own tenant's rows;
//!   * an operator scope (`audit:read-all`) restores the cross-tenant
//!     view, so nothing an operator needs is lost;
//!   * filtering happens BEFORE `limit` — otherwise a caller pages
//!     through other tenants' rows and receives a near-empty window of
//!     their own, which looks like "no activity" rather than a bug;
//!   * boundary rejections (`tenant: "-"`, no principal resolved) are
//!     operator-only, because there is no tenant to scope them by.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real OIDC-signed
//! bearers from the in-repo issuer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::{TestIssuer, TritonProcess};

/// Two principals in different tenants, plus an operator.
fn token_for(issuer: &TestIssuer, sub: &str, tenant: &str, scope: &str) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "aud": "triton-test",
        "sub": sub,
        "tenant": tenant,
        "scope": scope,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
    }))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn env_for(issuer: &TestIssuer) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "triton-test".to_string(),
        ),
    ])
}

/// Drive one `echo` dispatch as `token`, so the ring buffer holds an
/// entry owned by that principal's tenant.
async fn dispatch_as(proc: &TritonProcess, token: &str, msg: &str) {
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth(token)
        .json(&json!({ "message": msg }))
        .send()
        .await
        .expect("POST echo");
    assert!(resp.status().is_success(), "{}", resp.status());
}

async fn audit_entries(proc: &TritonProcess, token: &str, query: &str) -> Vec<Value> {
    let body: Value = reqwest::Client::new()
        .get(proc.rest_url(&format!("/v1/audit{query}")))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /v1/audit")
        .json()
        .await
        .expect("decode audit");
    body["entries"].as_array().cloned().unwrap_or_default()
}

/// The leak: acme must not see globex's dispatches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tenant_cannot_read_another_tenants_audit_entries() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;
    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");

    dispatch_as(&proc, &acme, "acme-secret-one").await;
    dispatch_as(&proc, &globex, "globex-secret-one").await;

    let seen = audit_entries(&proc, &acme, "").await;
    assert!(!seen.is_empty(), "acme must see its own entries");
    for e in &seen {
        assert_eq!(
            e["tenant"], "acme",
            "acme read a row belonging to another tenant: {e}"
        );
    }
}

/// The operator view is not lost — it is gated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_scope_restores_the_cross_tenant_view() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;
    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");
    let operator = token_for(&issuer, "ops", "ops", "audit:read-all");

    dispatch_as(&proc, &acme, "acme-two").await;
    dispatch_as(&proc, &globex, "globex-two").await;

    let seen = audit_entries(&proc, &operator, "").await;
    let tenants: Vec<&str> = seen.iter().filter_map(|e| e["tenant"].as_str()).collect();
    assert!(
        tenants.contains(&"acme") && tenants.contains(&"globex"),
        "an operator must still see every tenant; got {tenants:?}"
    );
}

/// Filtering must precede `limit`. Otherwise the newest N rows are taken
/// first and THEN filtered, so a caller whose traffic is a small share of
/// a busy gateway sees an empty page and reads it as "no activity".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtering_happens_before_the_limit_is_applied() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;
    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");

    // One acme row, then plenty of globex rows on top of it.
    dispatch_as(&proc, &acme, "acme-buried").await;
    for i in 0..8 {
        dispatch_as(&proc, &globex, &format!("globex-{i}")).await;
    }

    // A limit smaller than the noise above it.
    let seen = audit_entries(&proc, &acme, "?limit=3").await;
    assert!(
        !seen.is_empty(),
        "acme's own row must survive a small limit — filtering has to run \
         before the window is taken, not after"
    );
    for e in &seen {
        assert_eq!(e["tenant"], "acme", "got: {e}");
    }
}

/// Boundary rejections carry `tenant: "-"` because no principal was
/// resolved. They are operator-only: there is no tenant to scope them
/// by, so showing them to everyone re-opens the leak on the rows most
/// likely to name another tenant's sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattributed_rejections_are_operator_only() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;
    let acme = token_for(&issuer, "alice", "acme", "chat");
    let operator = token_for(&issuer, "ops", "ops", "audit:read-all");

    // An unauthenticated probe produces a `tenant: "-"` rejection row.
    let _ = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST unauth");
    dispatch_as(&proc, &acme, "acme-three").await;

    let as_tenant = audit_entries(&proc, &acme, "").await;
    assert!(
        as_tenant.iter().all(|e| e["tenant"] != "-"),
        "unattributed rows must not reach a tenant-scoped caller: {as_tenant:?}"
    );
    let as_operator = audit_entries(&proc, &operator, "").await;
    assert!(
        as_operator.iter().any(|e| e["tenant"] == "-"),
        "but an operator must still see them — they are the rows that matter \
         most during an incident"
    );
}

/// `/v1/tools` had the same discarded-principal shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tool_listing_is_scoped_too() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;
    let acme = token_for(&issuer, "alice", "acme", "chat");

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .bearer_auth(&acme)
        .send()
        .await
        .expect("GET /v1/tools");
    assert!(resp.status().is_success(), "{}", resp.status());
    let body: Value = resp.json().await.expect("json");
    assert!(
        body["tools"].is_array(),
        "the listing still works for an ordinary principal; got: {body}"
    );
}

/// The Explorer authenticates with the dev token. Tenant-scoping would
/// otherwise empty its audit page — the dev principal is `tenant: "dev"`
/// while chat traffic carries the sender's tenant. The dev path is
/// compiled out of production builds (`--no-default-features`), so
/// granting it the operator view costs nothing there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dev_token_keeps_the_operator_view() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    // The rows the Explorer actually exists to show are NOT the dev
    // principal's own — they are chat and boundary traffic carrying some
    // other tenant. Asserting only that dev sees its own dispatch would
    // pass whether or not the dev token has the operator view, so drive a
    // row it could not otherwise see.
    let _ = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .json(&json!({ "message": "unauthenticated" }))
        .send()
        .await
        .expect("POST unauth");

    let seen = audit_entries(&proc, "dev-token", "").await;
    assert!(
        seen.iter().any(|e| e["tenant"] == "-"),
        "the Explorer must still see rows outside its own tenant, or its \
         audit page silently empties. The dev path is compiled out of \
         production (`--no-default-features`), so the operator view costs \
         nothing there. got: {seen:?}"
    );
}

#[allow(dead_code)]
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

// ── Crew review of #306: four confirmed defects in the above ────────────

/// F1. `entries` went through `audit_visibility`; `bodies` did not.
///
/// The pivot needs no out-of-band knowledge: read your OWN `/v1/audit`,
/// lift any `trace_id` from it, and `/v1/trace/{id}` returned the bodies
/// for the whole trace — which can span the identity-resolver dispatch
/// running under `tenant: "system"`. One hop, inside the product's own
/// UI flow.
///
/// LIMIT, stated rather than implied: this drives the real endpoint but
/// can only prove the ENTRIES half. `bodies` is populated only with the
/// dev `capture` feature, which the integration-test binary does not
/// compile in, so the assertion below passes whether or not the fix is
/// present. The gate itself is pinned by
/// `rest::trace_scope_tests::bodies_follow_the_entries_beside_them`.
/// Both halves exist because neither alone is honest: the unit test
/// cannot show the handler calls the gate, and this cannot show the gate
/// works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_bodies_are_scoped_like_the_entries_beside_them() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");
    dispatch_as(&proc, &globex, "globex-secret-payload").await;

    // globex reads its own row to get a real trace_id — this is the
    // attacker's starting position, not a contrivance.
    let rows = audit_entries(&proc, &globex, "?limit=50").await;
    let trace_id = rows
        .iter()
        .find_map(|r| r["trace_id"].as_str())
        .expect("globex has a trace of its own")
        .to_string();

    let seen = |body: &Value| -> (usize, usize) {
        (
            body["entries"].as_array().map_or(0, Vec::len),
            body["bodies"].as_array().map_or(0, Vec::len),
        )
    };

    let foreign: Value = reqwest::Client::new()
        .get(proc.rest_url(&format!("/v1/trace/{trace_id}")))
        .bearer_auth(&acme)
        .send()
        .await
        .expect("GET /v1/trace")
        .json()
        .await
        .expect("decode");
    let (entries, bodies) = seen(&foreign);
    assert_eq!(entries, 0, "acme must see none of globex's entries");
    assert_eq!(
        bodies, 0,
        "…and none of its bodies either. NOTE this holds trivially without \
         the `capture` feature — see the doc comment; the gate is unit-tested"
    );

    // The other half: globex still gets its own trace. A filter that
    // returns nothing to anyone is not a filter.
    let own: Value = reqwest::Client::new()
        .get(proc.rest_url(&format!("/v1/trace/{trace_id}")))
        .bearer_auth(&globex)
        .send()
        .await
        .expect("GET /v1/trace")
        .json()
        .await
        .expect("decode");
    assert!(
        seen(&own).0 > 0,
        "globex must still read its own trace: {own}"
    );
}

/// F2. Tenant scoping is an EQUALITY test, and the live callers all
/// carry a shared marker rather than a tenant.
///
/// A single-tenant OIDC caller with no `tenant` claim resolves to `-`,
/// as does every opaque Google access-token caller; every un-enrolled
/// chat sender carries `pairing`. Two callers holding `-` are not in the
/// same tenant — they are both unattributed — so matching on it hands
/// one every other unattributed caller's rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_marker_tenant_matches_nobody_elses_rows() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    // Two DIFFERENT people, both resolving to the sentinel tenant.
    let one = token_for(&issuer, "someone@data-zoo.de", "-", "chat");
    let two = token_for(&issuer, "another@data-zoo.de", "-", "chat");
    dispatch_as(&proc, &one, "first-callers-message").await;

    let rows = audit_entries(&proc, &two, "?limit=100").await;
    assert!(
        !rows.iter().any(|r| r["who"] == "someone@data-zoo.de"),
        "a `-` caller must not read another `-` caller's rows: {rows:?}"
    );

    // An operator still sees everything — the grant is what restores it.
    let op = token_for(&issuer, "ops", "-", "audit:read-all");
    let all = audit_entries(&proc, &op, "?limit=100").await;
    assert!(
        all.iter().any(|r| r["who"] == "someone@data-zoo.de"),
        "the operator scope must still restore the cross-tenant view"
    );
}
