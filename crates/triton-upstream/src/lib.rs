//! Upstream dispatch — resolve a tool name to a fixed `host:port` from
//! a static map and POST the args there (FR-U-1..5). Replaces the
//! in-process tool path for tools the registry doesn't carry.
//!
//! The Consul-discovery + Vault per-call OIDC-swap router was removed
//! when the substrate moved off the HashiCorp stack to Kamal; the only
//! discovery mechanism is now the static map in [`StaticUpstream`],
//! with workload→workload auth via a per-call RS256 JWT (no Vault).
//!
//! The dispatcher (`triton-core`) is the single audit pivot (ADR-6): it
//! emits one `phase: dispatch` line per call around the
//! `UpstreamDispatch::invoke`, so adapters here never emit audit lines.

pub mod sse;
pub mod static_upstream;

pub use static_upstream::StaticUpstream;
