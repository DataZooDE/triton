//! `triton-core` — types and primitives shared across adapters,
//! dispatcher, identity, and audit.
//!
//! Walking-skeleton scope (PR 1): only the [`BuildInfo`] struct and
//! re-exports the binary uses to compose the `/healthz` handler.
//! Subsequent PRs add `Principal`, `ToolRegistry`, audit emitter,
//! error variants, and the upstream-router traits.

/// Static build information stamped into the binary at compile time.
///
/// `image_sha` is read from the `TRITON_IMAGE_SHA` env var at runtime;
/// the substrate's Packer step bakes the value into the Nomad job env
/// (see `architecture.md` §7).
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub binary_sha: &'static str,
    pub package_version: &'static str,
}

impl BuildInfo {
    pub const CURRENT: BuildInfo = BuildInfo {
        // Populated by build.rs in PR 3; placeholder for the skeleton.
        binary_sha: env!("CARGO_PKG_VERSION"),
        package_version: env!("CARGO_PKG_VERSION"),
    };
}
