//! `triton-core` — types and primitives shared across adapters,
//! dispatcher, identity, and audit.
//!
//! Scope so far:
//! * `RuntimeInfo` — the JSON-able struct backing `GET /version`
//!   (FR-O-2). [PR 3]
//! * `Principal`, `TritonError`, `Tool`, `ToolRegistry`, `Dispatcher`,
//!   `audit::*` — the dispatcher and audit story per ADR-6, FR-D-1..5,
//!   FR-AU-1..4. [PR 4]
//!
//! Subsequent PRs add the upstream-router trait (PR 9), the A2UI
//! builder seam (PR 10), and v0.2 surface-mapper types.

use serde::Serialize;

pub mod audit;
pub mod dispatcher;
pub mod error;
pub mod principal;
pub mod tool;

pub use dispatcher::{Dispatch, Dispatcher, envelope};
pub use error::TritonError;
pub use principal::{Principal, ToolPrincipal};
pub use tool::{Tool, ToolDescriptor, ToolRegistry};

/// A2UI envelope version requested by the caller (FR-A-3). Per ADR-4
/// the dispatcher and tools never branch on this; only the
/// envelope-builder layer (PR 10) consumes it. PR 5 just parses
/// and stores the requested version on the dispatcher response so
/// PR 10's builder can route to the right module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum A2uiVersion {
    V08,
    V09,
}

impl Default for A2uiVersion {
    /// Per FR-A-3: `Accept: application/json+a2ui` (no `version`
    /// parameter) negotiates v0.8.
    fn default() -> Self {
        A2uiVersion::V08
    }
}

/// Runtime metadata reported by `GET /version` (FR-O-2).
///
/// The spec MUSTs `binary_sha` + `image_sha` (architecture.md §7).
/// We additionally expose `env` and `package_version` because
/// operators reading `/version` at 3am want a single, at-a-glance
/// answer to "what is this process and which env does it think it's
/// in?" — neither field is secret. The two extras are *additive*
/// over the spec, not in place of it; if a client asks for stricter
/// contract conformance later, a follow-up PR can carve them into a
/// separate `/version/full` route.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeInfo {
    pub binary_sha: String,
    pub image_sha: Option<String>,
    pub env: String,
    pub package_version: String,
}
