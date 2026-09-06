//! #287 — the revocation lever must exist on the EMBEDDED surface too.
//!
//! Live verification on agent-lab caught this: `TRITON_DENIED_PRINCIPALS`
//! was wired in `triton-bin`, and the deployment that actually runs does
//! not use `triton-bin`. `dz-agent-template` embeds triton
//! (`AGENT_INGRESS=embedded`) and calls `Dispatcher::new` directly — in
//! three separate places. So the denylist deployed, the pod booted, and
//! nothing was revoked. No test caught it because every existing test
//! drove the standalone binary.
//!
//! That is the same failure #289 was about, committed by me: a security
//! control that each call site has to remember to opt into is not a
//! control, it is a suggestion. Three call sites today, and the fourth
//! one someone adds is where it goes wrong.
//!
//! So the denylist is read where the dispatcher is BUILT, not where some
//! hosts happen to wire it. This test builds a dispatcher the way an
//! embedded host does — `Dispatcher::new` and nothing else — and asserts
//! the revocation still applies.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::error::TritonError;
use triton_core::principal::{Principal, ToolPrincipal};
use triton_core::{Dispatcher, Tool, ToolRegistry};

/// Deliberately unique: `Dispatcher::new` reads the process environment,
/// and these tests share a process with several hundred others. A tenant
/// and subject nobody else uses cannot deny anyone else's principal.
const REVOKED_TENANT: &str = "embedded-denylist-test-tenant";
const REVOKED_SUB: &str = "embedded-denylist-test-subject";

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        Ok(json!({ "echoed": args }))
    }
}

fn principal(tenant: &str, sub: &str) -> Principal {
    Principal {
        sub: sub.to_string(),
        scopes: vec!["chat".to_string()],
        groups: Vec::new(),
        tenant: tenant.to_string(),
        raw_token: String::new(),
        trace_id: format!("test-{sub}"),
        sender_ref: None,
    }
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg
}

/// An embedded host builds its dispatcher with `Dispatcher::new` and no
/// builder calls. The revocation must still apply — otherwise the lever
/// exists only for a binary nobody runs in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedded_host_revokes_without_wiring_anything() {
    unsafe {
        std::env::set_var(
            "TRITON_DENIED_PRINCIPALS",
            format!("{REVOKED_TENANT}/{REVOKED_SUB}"),
        );
    }
    // Exactly what agent-core does — no `.with_denied_principals(...)`.
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test");

    let denied = dispatcher
        .invoke(
            "echo",
            json!({ "m": "hi" }),
            principal(REVOKED_TENANT, REVOKED_SUB),
            "rest",
        )
        .await;
    match denied {
        Err(TritonError::Forbidden(msg)) => {
            assert!(
                msg.contains(REVOKED_SUB),
                "the refusal should name the principal; got: {msg}"
            );
        }
        other => panic!("a revoked principal must be refused; got {other:?}"),
    }

    // …and only that principal. A kill switch that takes out the tenant
    // is not a kill switch, it is an outage.
    let sibling = dispatcher
        .invoke(
            "echo",
            json!({ "m": "hi" }),
            principal(REVOKED_TENANT, "someone-else"),
            "rest",
        )
        .await;
    assert!(
        sibling.is_ok(),
        "a sibling in the same tenant must be unaffected"
    );

    // The same subject in another tenant is a different person.
    let other_tenant = dispatcher
        .invoke(
            "echo",
            json!({ "m": "hi" }),
            principal("some-other-tenant", REVOKED_SUB),
            "rest",
        )
        .await;
    assert!(
        other_tenant.is_ok(),
        "the same sub in another tenant must be unaffected"
    );

    unsafe {
        std::env::remove_var("TRITON_DENIED_PRINCIPALS");
    }
}

/// Streaming is a second entry point into the same dispatch, so a check
/// on one alone is no check — and an embedded A2A host uses this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_embedded_streaming_entry_point_revokes_too() {
    unsafe {
        std::env::set_var(
            "TRITON_DENIED_PRINCIPALS",
            format!("{REVOKED_TENANT}/{REVOKED_SUB}"),
        );
    }
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test");
    let denied = dispatcher
        .invoke_streaming(
            "echo",
            json!({ "m": "hi" }),
            principal(REVOKED_TENANT, REVOKED_SUB),
            "a2a",
            None,
        )
        .await;
    assert!(
        matches!(denied, Err(TritonError::Forbidden(_))),
        "invoke_streaming must refuse a revoked principal too"
    );
    unsafe {
        std::env::remove_var("TRITON_DENIED_PRINCIPALS");
    }
}

// ── #284 / #249 on the embedded surface ─────────────────────────────────
//
// Tracing why the denylist never reached agent-lab turned up two more
// controls with the same shape: `can_invoke`'s scope restriction (#284)
// and the anonymous-rejection coalescing window (#249) were both applied
// by `triton-bin` and by nobody else.
//
// Neither is broken on agent-lab today — its Chat adapter sets
// `pairing_tool: None` deliberately, so there is no restriction to lose.
// But "nothing is broken today" is not the property these tests are for.
// An embedded host that DOES name a pairing tool would mint restricted
// principals and then forget the restriction, which is the exact bug
// #284 was opened to fix.

const PAIRED_TENANT: &str = "embedded-pairing-test-tenant";

fn pairing_principal() -> Principal {
    Principal {
        sub: "users/unenrolled".to_string(),
        // Holding ONLY `pairing` is the whole test: an enrolled principal
        // carries real scopes and is deliberately unaffected.
        scopes: vec!["pairing".to_string()],
        groups: Vec::new(),
        tenant: PAIRED_TENANT.to_string(),
        raw_token: String::new(),
        trace_id: "test-pairing".to_string(),
        sender_ref: None,
    }
}

/// An un-enrolled sender reaches the one tool the deployment named, and
/// nothing else — on a dispatcher built the way an embedded host builds
/// one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedded_host_honours_the_pairing_restriction() {
    unsafe {
        std::env::set_var("TRITON_PAIRING_TOOLS", "pair");
    }
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test");

    // `echo` is not the pairing tool, so a pairing-only principal cannot
    // reach it. Before this, they could reach anything.
    let denied = dispatcher
        .invoke("echo", json!({}), pairing_principal(), "rest")
        .await;
    assert!(
        matches!(denied, Err(TritonError::Forbidden(_))),
        "a pairing-only principal must not reach a tool outside the \
         restriction; got {denied:?}"
    );

    // An enrolled principal in the same tenant is unaffected — enrolment
    // itself lifts the restriction, so there is no second mechanism to
    // keep in sync.
    let enrolled = dispatcher
        .invoke("echo", json!({}), principal(PAIRED_TENANT, "alice"), "rest")
        .await;
    assert!(enrolled.is_ok(), "an enrolled principal must be unaffected");

    unsafe {
        std::env::remove_var("TRITON_PAIRING_TOOLS");
    }
}

/// The coalescing window (#249) likewise. Its value is audit-volume
/// tuning rather than a security control, but a host that cannot set it
/// cannot defend its own ring buffer from a scanner — and the embedded
/// surface is the one exposed to the public internet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedded_host_can_set_the_rejection_window() {
    unsafe {
        std::env::set_var("TRITON_AUDIT_REJECT_WINDOW_SECS", "0");
    }
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test");
    assert_eq!(
        dispatcher.reject_window_secs(),
        0,
        "an embedded host must be able to disable coalescing"
    );
    unsafe {
        std::env::set_var("TRITON_AUDIT_REJECT_WINDOW_SECS", "120");
    }
    let tuned = Dispatcher::new(Arc::new(registry()), "test");
    assert_eq!(tuned.reject_window_secs(), 120);

    // A junk value falls back to the default rather than failing boot:
    // this knob must never be the reason a gateway will not start.
    unsafe {
        std::env::set_var("TRITON_AUDIT_REJECT_WINDOW_SECS", "not-a-number");
    }
    let fallback = Dispatcher::new(Arc::new(registry()), "test");
    assert_eq!(
        fallback.reject_window_secs(),
        triton_core::dispatcher::DEFAULT_REJECT_WINDOW.as_secs()
    );
    unsafe {
        std::env::remove_var("TRITON_AUDIT_REJECT_WINDOW_SECS");
    }
}
