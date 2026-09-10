//! #282 — outside `local`, the cross-tenant audit view needs BOTH halves.
//!
//! `audit_visibility_in` computes
//!
//! ```text
//! operator = holds the `audit:read-all` SCOPE
//!            AND is named in TRITON_AUDIT_OPERATORS
//! ```
//!
//! and the two halves answer to different authorities on purpose: the
//! scope is a claim only the ISSUER can mint, the list is writable only
//! by the DEPLOYMENT. Neither alone hands out everyone's audit trail.
//! In `local` the scope suffices, mirroring the dev-token gate (ADR-10),
//! so a dev loop can read its own trail without an env var.
//!
//! **Why this file exists.** No integration test set
//! `TRITON_AUDIT_OPERATORS` at all, so the conjunction's non-local
//! behaviour was pinned by nothing — `audit_tenant_scope.rs` runs every
//! case with `TRITON_ENV=local`, where the left half short-circuits the
//! right. Two live consequences followed:
//!
//!   * On `lab`, `TRITON_AUDIT_OPERATORS` was set to a Google-OIDC
//!     caller's `(tenant, sub)` to grant them the operator view. It
//!     granted nothing — a Google ID token carries no `scp`/`scope`
//!     claim, so the left half is never true for that issuer — while
//!     silencing the boot warning that says nobody holds the view.
//!     `GET /v1/audit` returned `{"entries":[]}` to the named operator
//!     while the pod's stream carried two tenants
//!     (DataZooDE/hetzner-agent-substrate#805, ADR-0024).
//!   * The reverse shortcut — granting the scope from the deployment
//!     side — would collapse the conjunction to `named && named`. The
//!     tests below are what make that collapse visible: delete either
//!     half of the `&&` and one of them fails.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real OIDC-signed
//! bearers from the in-repo issuer.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::{TestIssuer, TritonProcess};

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

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

/// `nonprod`, NOT `local` — the whole point. `operators` is the literal
/// `TRITON_AUDIT_OPERATORS` value, or `None` to leave it unset.
fn env_nonprod(issuer: &TestIssuer, operators: Option<&str>) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "triton-test".to_string(),
        ),
    ]);
    if let Some(v) = operators {
        env.insert("TRITON_AUDIT_OPERATORS".to_string(), v.to_string());
    }
    env
}

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

/// The tenants visible to `token`.
///
/// Every step here fails LOUDLY on purpose. The first version sent the
/// GET, decoded without checking the status, and did
/// `unwrap_or_default()` on a missing `entries` — so a 500, a 401 or a
/// renamed field all produced an empty vec, which is precisely what the
/// two negative tests below assert. They would have passed while proving
/// nothing (caught by a crew review, in the file whose own header
/// complains about tests that cannot fail).
async fn audit_tenants(proc: &TritonProcess, token: &str) -> Vec<String> {
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/audit?limit=200"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /v1/audit");
    assert_eq!(
        resp.status(),
        200,
        "an authenticated caller must reach /v1/audit; an error here would \
         otherwise read as `no rows visible`"
    );
    let body: Value = resp.json().await.expect("decode audit");
    body["entries"]
        .as_array()
        .expect("audit response must carry an `entries` array")
        .iter()
        .filter_map(|e| e["tenant"].as_str().map(str::to_string))
        .collect()
}

/// Left half alone is not enough: a token the ISSUER minted with
/// `audit:read-all`, for a subject the DEPLOYMENT never named, sees only
/// its own rows.
///
/// This is the property that stops a compromised or over-generous issuer
/// from minting itself the cross-tenant view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scope_alone_does_not_grant_the_cross_tenant_view() {
    let issuer = TestIssuer::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_nonprod(&issuer, None)).await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");
    // Scope present, name absent.
    let claimant = token_for(&issuer, "ops", "ops", "audit:read-all");

    dispatch_as(&proc, &acme, "acme-one").await;
    dispatch_as(&proc, &globex, "globex-one").await;
    // The claimant dispatches too, so "sees nothing" cannot masquerade as
    // "correctly filtered".
    dispatch_as(&proc, &claimant, "ops-one").await;

    let seen = audit_tenants(&proc, &claimant).await;
    assert!(
        seen.iter().any(|t| t == "ops"),
        "the claimant must still see their OWN rows — an empty result \
         would satisfy the assertion below without proving anything; got \
         {seen:?}"
    );
    assert!(
        !seen.iter().any(|t| t == "acme" || t == "globex"),
        "an unnamed caller holding `audit:read-all` must NOT see other \
         tenants outside `local`; got {seen:?}"
    );
}

/// Right half alone is not enough either: naming a subject that does not
/// hold the scope grants nothing.
///
/// This is the exact configuration that was live on `lab` — a
/// Google-OIDC caller, who cannot carry a scope claim, named in
/// `TRITON_AUDIT_OPERATORS`. It reads as a grant and is not one, and the
/// only thing setting it accomplished was to silence the boot warning
/// that said so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naming_a_principal_without_the_scope_grants_nothing() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_nonprod(&issuer, Some("ops/ops")),
    )
    .await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");
    // Named in the list, but the issuer minted no `audit:read-all`.
    let named_only = token_for(&issuer, "ops", "ops", "chat");

    dispatch_as(&proc, &acme, "acme-two").await;
    dispatch_as(&proc, &globex, "globex-two").await;
    dispatch_as(&proc, &named_only, "ops-two").await;

    let seen = audit_tenants(&proc, &named_only).await;
    assert!(
        seen.iter().any(|t| t == "ops"),
        "the named caller must still see their OWN rows, or the assertion \
         below proves nothing; got {seen:?}"
    );
    assert!(
        !seen.iter().any(|t| t == "acme" || t == "globex"),
        "being named in TRITON_AUDIT_OPERATORS must NOT by itself grant \
         the cross-tenant view; got {seen:?}"
    );
}

/// Both halves together do grant it — otherwise the gate is not a gate,
/// it is a wall, and an operator has no way through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_plus_naming_grants_the_cross_tenant_view() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_nonprod(&issuer, Some("ops/ops")),
    )
    .await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    let globex = token_for(&issuer, "bob", "globex", "chat");
    let operator = token_for(&issuer, "ops", "ops", "audit:read-all");

    dispatch_as(&proc, &acme, "acme-three").await;
    dispatch_as(&proc, &globex, "globex-three").await;

    let seen = audit_tenants(&proc, &operator).await;
    assert!(
        seen.iter().any(|t| t == "acme") && seen.iter().any(|t| t == "globex"),
        "scope AND naming must restore the cross-tenant view; got {seen:?}"
    );
}

/// The list is a set of PAIRS, not of subjects. A caller with the right
/// `sub` in the wrong tenant is a different principal and is not the
/// named operator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_list_matches_the_tenant_too() {
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_nonprod(&issuer, Some("ops/ops")),
    )
    .await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    // Same `sub`, different tenant.
    let impostor = token_for(&issuer, "ops", "acme", "audit:read-all");

    dispatch_as(&proc, &acme, "acme-four").await;

    // Put an UNATTRIBUTED row in the buffer. Without one the assertion
    // below is vacuous — there is no operator-only row to fail to see,
    // so a matcher that ignored the tenant would pass unnoticed (found
    // by mutation: `operators.iter().any(|(_, sub)| ...)` survived).
    let unauth = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .send()
        .await
        .expect("GET /v1/tools unauthenticated");
    assert_eq!(unauth.status(), 401, "expected an unattributed rejection");

    let seen = audit_tenants(&proc, &impostor).await;
    // They legitimately see `acme` — that is their OWN tenant here — so
    // the assertion is that the naming did not promote them: an operator
    // would also see the boundary rejections carrying `-`.
    assert!(
        !seen.iter().any(|t| t == "-"),
        "`ops/ops` must not match `ops` in tenant `acme`; an unattributed \
         row is operator-only, so seeing one means the pair matched on the \
         subject alone; got {seen:?}"
    );
}

// There is deliberately no `/v1/trace` bodies test here, though a crew
// review asked for one.
//
// `/v1/trace` gates its `bodies` array on `may_read_trace`, but
// `triton_core::trace::captured` is behind the `capture` cargo feature —
// off by default and ALWAYS off in release — so the spawned binary these
// tests drive returns an empty `bodies` array to everyone. A test
// asserting "an unnamed scope-holder sees no bodies" would pass with
// `may_read_trace` deleted entirely: vacuous by construction, which is
// the failure this file exists to avoid. (Written, run, and removed on
// discovering the owner sees nothing either.)
//
// The observable surface for `may_read_trace` is A2A `tasks/get`, which
// gates on it without needing captured bodies. That is covered in
// `a2a_trace_authz.rs`, on the embedded host. Pinning `/v1/trace` itself
// needs the harness to build the binary with `--features capture`, which
// is a change to the harness, not to this file.

/// The default is `local`, and `local` turns the conjunction OFF.
///
/// `TRITON_ENV` is `#[arg(long, env = "TRITON_ENV", default_value =
/// "local")]`, and `is_named_operator` treats `local` as "named". So a
/// deployment that simply never sets the variable — the default path, not
/// a typo — hands the cross-tenant view to anyone whose issuer will mint
/// `audit:read-all`. Every other test in this file sets `nonprod`
/// explicitly, so nothing pinned the default.
///
/// This test does not argue the default should change; it records what it
/// currently IS, so a change is a deliberate act with a failing test
/// attached rather than a silent one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_unset_env_default_grants_the_view_on_the_scope_alone() {
    let issuer = TestIssuer::start().await;
    // Deliberately NO TRITON_ENV.
    let env = HashMap::from([
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "triton-test".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let acme = token_for(&issuer, "alice", "acme", "chat");
    let claimant = token_for(&issuer, "ops", "ops", "audit:read-all");

    dispatch_as(&proc, &acme, "acme-default").await;
    dispatch_as(&proc, &claimant, "ops-default").await;

    let seen = audit_tenants(&proc, &claimant).await;
    assert!(
        seen.iter().any(|t| t == "acme"),
        "with TRITON_ENV unset the binary defaults to `local`, where the \
         scope ALONE grants the cross-tenant view. If this assertion \
         starts failing, the default changed — which may well be the right \
         call, but it is a behaviour change and this is the test that says \
         so; got {seen:?}"
    );
}
