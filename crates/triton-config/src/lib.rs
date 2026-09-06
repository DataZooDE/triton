//! Deployment-owned authorization config, read from the environment.
//!
//! One home, and that is the point. These values were read in five places
//! across three crates — `triton-core` parsed two of them inside
//! `Dispatcher::new`, `triton-bin` parsed a third, `triton-adapters-http`
//! re-read a fourth on every request — and the scatter is what produced
//! the same defect three times running: a lever documented in one place,
//! read in another, and enforced in neither.
//!
//! The rule this crate exists to enforce: **anything the DEPLOYMENT
//! decides about who may do what is read here, once, at boot.**
//! `triton-core` stays env-free; hosts pass the result in.

use std::collections::HashSet;
use std::time::Duration;

use triton_core::dispatcher::{DEFAULT_REJECT_WINDOW, DispatchControls, parse_principal_set};

/// Everything a host needs to configure its authorization surfaces.
///
/// Deliberately one struct rather than several: the previous shape had a
/// host assembling controls from three sources and forgetting a fourth,
/// and a single value that is either passed or not passed is much harder
/// to half-configure.
pub struct DeploymentConfig {
    /// Passed to `Dispatcher::new`.
    pub controls: DispatchControls,
    /// Principals the deployment names as operators, `(tenant, sub)`.
    ///
    /// Parsed ONCE here rather than per request. It used to be re-read
    /// and re-parsed inside `audit_visibility`, which meant a malformed
    /// entry printed a warning on every `/v1/audit` call — and, because
    /// it borrowed the denylist parser, a warning naming the wrong
    /// subsystem, sending the operator to hunt through their denylist.
    pub audit_operators: HashSet<(String, String)>,
}

impl DeploymentConfig {
    /// Read the whole set from the process environment.
    ///
    /// * `TRITON_DENIED_PRINCIPALS` — `tenant/sub`, revoked (#287)
    /// * `TRITON_PAIRING_TOOLS` — tools an un-enrolled sender may reach (#284)
    /// * `TRITON_AUDIT_REJECT_WINDOW_SECS` — rejection-audit coalescing (#249)
    /// * `TRITON_AUDIT_OPERATORS` — `tenant/sub`, cross-tenant audit view
    ///
    /// A junk reject-window falls back to the default rather than failing
    /// boot: that knob must never be the reason a gateway will not start.
    /// The two principal lists fail closed instead — a malformed entry is
    /// dropped with a warning naming it, so a typo revokes nobody and
    /// grants nobody rather than guessing.
    pub fn from_env() -> Self {
        let mut controls = DispatchControls::unenforced()
            .extend_denied_principals(&env("TRITON_DENIED_PRINCIPALS"));
        let tools: Vec<String> = env("TRITON_PAIRING_TOOLS")
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        controls = controls.replace_scope_restriction("pairing", tools);
        if let Some(secs) = std::env::var("TRITON_AUDIT_REJECT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            controls = controls.with_reject_window(Duration::from_secs(secs));
        }
        Self {
            controls,
            audit_operators: parse_principal_set(&env("TRITON_AUDIT_OPERATORS"), "audit operator"),
        }
    }
}

/// Say what is in force, once, from one place — for whichever host is
/// running. Announcing from a host's `main` is how the revocation lever
/// came to be enforced on one surface and reported on another.
///
/// `env_name` is the RESOLVED deployment environment, not
/// `std::env::var("TRITON_ENV")`: clap reads that variable into
/// `Settings` without ever setting it, so a deployment configured with
/// `--env prod` leaves it unset and looks local.
pub fn announce(
    env_name: &str,
    audit_operators: &HashSet<(String, String)>,
    denied: impl Iterator<Item = String>,
    enforcing: bool,
) {
    let mut denied: Vec<String> = denied.collect();
    if !denied.is_empty() {
        denied.sort_unstable();
        eprintln!(
            // The wording names the surfaces actually covered. It used
            // to say "every dispatch, proactive send and audit read",
            // which overstated it: `deny_if_revoked` guards /v1/outbound,
            // /v1/audit, /v1/trace and tasks/get, while /v1/tools,
            // /v1/manifest, /v1/metrics, /v1/surface/render and the MCP
            // listing arms verify a principal and do not consult the
            // denylist. An operator mid-incident must not believe more is
            // closed than is. (#306 crew F2.)
            "WARN denylist active: {} principal(s) revoked — every tool DISPATCH \
             is refused 403, as are /v1/outbound, /v1/audit, /v1/trace and \
             a2a tasks/get. Read-only listings (/v1/tools, /v1/manifest, \
             /v1/metrics, /v1/surface/render, MCP tools/list) still answer a \
             revoked principal (#287): {}",
            denied.len(),
            denied.join(", ")
        );
    }
    if env_name != "local" && audit_operators.is_empty() {
        eprintln!(
            "WARN TRITON_AUDIT_OPERATORS is unset: NOBODY holds the cross-tenant view \
             of /v1/audit or /v1/trace. The `audit:read-all` scope alone no longer \
             grants it, because that claim namespace belongs to the issuer rather \
             than to this deployment. Set it to a comma-separated list of `tenant/sub`."
        );
    }
    if env_name != "local" && !enforcing {
        eprintln!(
            "WARN dispatcher built with UNENFORCED controls in env `{env_name}`: no \
             principal is revoked and no scope is restricted. That is a valid \
             configuration and may be intended — but in a non-local deployment it is \
             more often a host that forgot to pass DeploymentConfig::from_env()."
        );
    }
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// Re-exported so a host needs one import.
pub use triton_core::dispatcher::DispatchControls as Controls;

/// The default coalescing window, re-exported for hosts that report it.
pub const DEFAULT_WINDOW: Duration = DEFAULT_REJECT_WINDOW;
