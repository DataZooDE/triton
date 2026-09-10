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
use triton_config::DeploymentConfig;
use triton_core::dispatcher::DispatchControls;
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
        conversation_ref: None,
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
    let dispatcher = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );

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
    let dispatcher = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );
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
        conversation_ref: None,
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
    let dispatcher = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );

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
    let dispatcher = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );
    assert_eq!(
        dispatcher.reject_window_secs(),
        0,
        "an embedded host must be able to disable coalescing"
    );
    unsafe {
        std::env::set_var("TRITON_AUDIT_REJECT_WINDOW_SECS", "120");
    }
    let tuned = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );
    assert_eq!(tuned.reject_window_secs(), 120);

    // A junk value falls back to the default rather than failing boot:
    // this knob must never be the reason a gateway will not start.
    unsafe {
        std::env::set_var("TRITON_AUDIT_REJECT_WINDOW_SECS", "not-a-number");
    }
    let fallback = Dispatcher::new(
        Arc::new(registry()),
        "test",
        DeploymentConfig::from_env().controls,
    );
    assert_eq!(
        fallback.reject_window_secs(),
        triton_core::dispatcher::DEFAULT_REJECT_WINDOW.as_secs()
    );
    unsafe {
        std::env::remove_var("TRITON_AUDIT_REJECT_WINDOW_SECS");
    }
}

/// An operator must be able to SEE the lever is engaged.
///
/// Verified on agent-lab the hard way: the revocation worked and nothing
/// announced it, because the boot warning lived in `triton-bin`'s main
/// and an embedded host does not run that. A control you cannot confirm
/// is engaged is one you will not trust — or worse, will assume is
/// engaged when a typo dropped every entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_denylist_announces_itself() {
    let bin = {
        let mut here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let cand = here.join("target/debug/triton");
            if cand.exists() {
                break cand;
            }
            assert!(here.pop(), "triton binary not found");
        }
    };
    // Drive the real process so this covers what a deployment sees, and
    // include a bare entry: the count must reflect what was ACCEPTED, or
    // the log reassures an operator about a revocation that never
    // happened.
    let out = std::process::Command::new(bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_DENIED_PRINCIPALS", "acme/alice, bare-subject")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let pid = out.id();
    std::thread::sleep(std::time::Duration::from_millis(700));
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let done = out.wait_with_output().expect("wait");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(
        log.contains("denylist active") && log.contains("acme/alice"),
        "an active denylist must announce itself and name who it revoked; got:\n{log}"
    );
    // The announcement must describe what is ACTUALLY refused, and the
    // wording has been wrong in both directions: it first claimed only
    // "every dispatch" while /v1/outbound bypassed the check, then
    // claimed dispatches + sends + audit reads while /v1/tools,
    // /v1/manifest and the MCP listings still answered a revoked caller.
    // An operator mid-incident must not believe more is closed than is.
    assert!(
        log.contains("/v1/outbound") && log.contains("tasks/get"),
        "the announcement must NAME the guarded surfaces; got:\n{log}"
    );
    assert!(
        log.contains("still answer") && log.contains("/v1/tools"),
        "…and must name the ones that still answer a revoked principal, \
         because overstating coverage is the failure mode here; got:\n{log}"
    );
    assert!(
        log.contains("1 principal"),
        "the count must be what was ACCEPTED, not what was typed — the bare \
         entry was dropped; got:\n{log}"
    );
    assert!(
        log.contains("entry `bare-subject` ignored"),
        "and the dropped entry must be named; got:\n{log}"
    );
}

/// Crew review of #306, F13. `with_denied_principals` used to REPLACE
/// the environment's set while the boot announcement fired inside
/// `new()`, before any override — so a host calling the builder logged
/// one denylist and enforced another.
///
/// Both halves of that are now structurally impossible: the controls are
/// a REQUIRED constructor parameter, so there is no builder to disagree
/// with, and the announcement happens where the dispatcher is finished
/// being built rather than where its config is read.
///
/// What remains testable, and what this pins: the two sources compose in
/// the safe direction. A denylist is a DENY-set, so merging can only
/// revoke MORE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denylist_sources_compose_by_revoking_more_never_less() {
    let controls = DispatchControls::unenforced()
        .extend_denied_principals("env-tenant/env-sub")
        .extend_denied_principals("code-tenant/code-sub");
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test", controls);

    for (tenant, sub) in [("env-tenant", "env-sub"), ("code-tenant", "code-sub")] {
        let denied = dispatcher
            .invoke("echo", json!({}), principal(tenant, sub), "rest")
            .await;
        assert!(
            matches!(denied, Err(TritonError::Forbidden(_))),
            "`{tenant}/{sub}` came from one of two sources and both must be in force"
        );
    }
    assert_eq!(dispatcher.denied_principals().count(), 2);
}

/// A scope restriction is an ALLOW-set inside a gate, so the opposite
/// rule applies: merging could only WIDEN what a restricted principal
/// reaches, which is why `restrict_scope` REPLACES and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scope_restriction_replaces_rather_than_widening() {
    let controls = DispatchControls::unenforced()
        .replace_scope_restriction("pairing", ["stale_env_tool".to_string()])
        .replace_scope_restriction("pairing", ["echo".to_string()]);
    let dispatcher = Dispatcher::new(Arc::new(registry()), "test", controls);

    // The later source won outright: `echo` is reachable...
    assert!(
        dispatcher
            .invoke("echo", json!({}), pairing_principal(), "rest")
            .await
            .is_ok(),
        "the surviving restriction must permit its own tool"
    );
    // ...and the stale entry did not survive as a union. A merge here
    // would silently keep another adapter's enrolment tool reachable by
    // every un-enrolled sender, forever.
    let restrictions: Vec<String> = dispatcher.restricted_tools("pairing").collect();
    assert_eq!(
        restrictions,
        vec!["echo".to_string()],
        "a stale entry must not survive as a union"
    );
}
