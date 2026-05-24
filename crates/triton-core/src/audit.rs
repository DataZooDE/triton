//! One JSON line on stdout per dispatcher invocation (FR-AU-1).
//! Substrate audit-collector tails alloc stdout (G-S3, Δ-3); the
//! binary never ships audit lines itself — explicitly forbidden by
//! ADR-7 and FR-AU-4.
//!
//! Schema is the experiments' superset extended with the substrate
//! `{who, what, when, env, result}` fields per FR-AU-2. Tokens,
//! JWKS private material, and Vault-minted upstream tokens MUST
//! NEVER appear (FR-AU-3); the manual `Debug` on [`crate::Principal`]
//! plus this schema's lack of any token field is the enforcement.

use std::io::Write;

use serde::Serialize;

/// Discriminator on each audit record. PR 4 emits `Dispatch` and
/// `Rejected`; PR 9 adds `Upstream`. v0.2 chat-channel adapters
/// (PR 12+) add `Post`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditPhase {
    /// Successful dispatch through the tool — the normal happy path.
    Dispatch,
    /// Inbound rejected at the boundary (auth, malformed body,
    /// signature). Per ADR-15: a `phase: rejected` line is emitted
    /// *before the dispatcher is reached* — but constructed by the
    /// dispatcher so adapters never own the schema (ADR-6).
    Rejected,
    /// Outbound dispatch to an upstream agent.
    Upstream,
    /// Chat-channel outbound post-back (PR 18). The dispatcher's
    /// `record_post` emits this after the adapter ships the tool
    /// result back to the platform (Telegram, Discord, ...).
    Post,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord<'a> {
    /// Always `"audit"` — discriminator the substrate audit
    /// collector uses to separate audit from `kind: "log"` lines on
    /// the same stdout (architecture.md §8.2).
    pub kind: &'static str,
    pub phase: AuditPhase,
    pub when: String,
    pub who: &'a str,
    pub what: &'a str,
    pub env: &'a str,
    pub result: String,
    pub protocol: &'a str,
    pub tool: &'a str,
    pub subject: &'a str,
    pub tenant: &'a str,
    pub latency_ms: u64,
    pub status: u16,
    pub trace_id: &'a str,
}

/// Emit one audit line to stdout. `stdout().lock()` plus an explicit
/// flush survives concurrent emission from multiple in-flight
/// requests (the lock serialises) and SIGTERM drain (`flush` makes
/// sure the line clears the FILE* buffer before the kernel pipe).
pub fn emit(record: &AuditRecord<'_>) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    // Best-effort: if stdout fails, we cannot meaningfully recover
    // (audit collector cares about the line, not our reaction).
    let _ = handle.write_all(line.as_bytes());
    let _ = handle.write_all(b"\n");
    let _ = handle.flush();
}

/// RFC 3339 UTC timestamp with trailing `Z` and microsecond
/// precision — what the substrate audit collector expects.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
