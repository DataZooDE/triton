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

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

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

/// Closed-set disposition of a chat post-back (FR-AU-1 v0.2). This
/// is the *only* thing `status_label` may carry; any finer-grained
/// reason (a modal was opened, a dashboard was rasterized) rides on
/// the separate `status_detail` field so the substrate collector
/// sees a uniform discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// The tool result reached the platform.
    Posted,
    /// Transient failure; the adapter will retry.
    Retry,
    /// Giving up — the result was not delivered.
    Dropped,
}

impl PostOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            PostOutcome::Posted => "posted",
            PostOutcome::Retry => "retry",
            PostOutcome::Dropped => "dropped",
        }
    }
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
    /// FR-AU-1 v0.2 closed-set discriminator for chat post audits:
    /// `{"posted", "retry", "dropped"}`. Omitted on non-chat-post
    /// phases (dispatch/rejected/upstream) so the schema stays
    /// uniform across the substrate audit collector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_label: Option<&'static str>,
    /// Optional finer-grained reason behind `status_label` (e.g.
    /// `modal_opened`, `rasterizer_call`, `rasterizer_failed`). Not a
    /// closed set; for dashboards/diagnosis only. Omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<&'static str>,
    pub trace_id: &'a str,
}

/// Emit one audit line to stdout. `stdout().lock()` plus an explicit
/// flush survives concurrent emission from multiple in-flight
/// requests (the lock serialises) and SIGTERM drain (`flush` makes
/// sure the line clears the FILE* buffer before the kernel pipe).
///
/// Also pushes an owned copy of the record into the in-process ring
/// buffer (FR-AU-5) so a tailnet-only operator endpoint can serve
/// the recent history without scraping the substrate's log shipper.
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
    drop(handle);
    AuditBuffer::push(record);
}

/// In-process bounded history of recent audit records, exposed via
/// the REST adapter for the explorer's Audit page. The buffer lives
/// in memory only — restart-clean by construction (G-8). Capacity
/// chosen so a busy gateway covers ≈10 min of history before the
/// oldest entries scroll off; operators investigating cross-process
/// failures still go to the substrate log shipper.
pub const AUDIT_BUFFER_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub kind: &'static str,
    pub phase: AuditPhase,
    pub when: String,
    pub who: String,
    pub what: String,
    pub env: String,
    pub result: String,
    pub protocol: String,
    pub tool: String,
    pub subject: String,
    pub tenant: String,
    pub latency_ms: u64,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_label: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<&'static str>,
    pub trace_id: String,
}

impl<'a> From<&AuditRecord<'a>> for AuditEntry {
    fn from(r: &AuditRecord<'a>) -> Self {
        Self {
            kind: r.kind,
            phase: r.phase,
            when: r.when.clone(),
            who: r.who.to_string(),
            what: r.what.to_string(),
            env: r.env.to_string(),
            result: r.result.clone(),
            protocol: r.protocol.to_string(),
            tool: r.tool.to_string(),
            subject: r.subject.to_string(),
            tenant: r.tenant.to_string(),
            latency_ms: r.latency_ms,
            status: r.status,
            status_label: r.status_label,
            status_detail: r.status_detail,
            trace_id: r.trace_id.to_string(),
        }
    }
}

pub struct AuditBuffer {
    inner: Mutex<VecDeque<AuditEntry>>,
}

impl AuditBuffer {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(AUDIT_BUFFER_CAPACITY)),
        }
    }

    fn global() -> &'static Self {
        static BUF: OnceLock<AuditBuffer> = OnceLock::new();
        BUF.get_or_init(AuditBuffer::new)
    }

    fn push(record: &AuditRecord<'_>) {
        let entry: AuditEntry = record.into();
        let buf = Self::global();
        let mut q = buf.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == AUDIT_BUFFER_CAPACITY {
            q.pop_front();
        }
        q.push_back(entry);
    }

    /// Return the most recent `limit` entries (newest first),
    /// optionally filtered to entries whose `trace_id` matches.
    pub fn recent(limit: usize, trace_id: Option<&str>) -> Vec<AuditEntry> {
        let buf = Self::global();
        let q = buf.inner.lock().unwrap_or_else(|e| e.into_inner());
        let it = q.iter().rev();
        let filtered: Box<dyn Iterator<Item = &AuditEntry>> = match trace_id {
            Some(t) => Box::new(it.filter(move |e| e.trace_id == t)),
            None => Box::new(it),
        };
        filtered.take(limit).cloned().collect()
    }

    /// Reset for tests. Not exposed in production — the buffer is
    /// process-lifetime by construction (G-8) and the dispatcher
    /// must never see a stale read.
    #[cfg(test)]
    pub fn clear() {
        let buf = Self::global();
        buf.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// RFC 3339 UTC timestamp with trailing `Z` and microsecond
/// precision — what the substrate audit collector expects.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
