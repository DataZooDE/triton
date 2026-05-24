//! `triton-core` — types and primitives shared across adapters,
//! dispatcher, identity, and audit.
//!
//! Walking scope so far: [`RuntimeInfo`], the JSON-able struct
//! that backs `GET /version` (FR-O-2). Subsequent PRs add
//! `Principal`, `ToolRegistry`, audit emitter, error variants, and
//! the upstream-router traits.

use serde::Serialize;

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
