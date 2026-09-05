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
    /// WHY a request was refused, in the error's own words.
    ///
    /// `result` carries only the error CLASS (`error:auth`), which makes
    /// every rejection look alike: a wrong issuer, an unknown signing
    /// key, an untrusted reply URL and a missing header are one string.
    /// That cost two live debugging cycles — a silent Google Chat 401 on
    /// 2026-08-23 and a silent Teams 401 on 2026-08-29 — where the only
    /// way to tell them apart was reading adapter source.
    ///
    /// `String`, not `&'static str` like `status_detail` above, precisely
    /// because the useful part is dynamic: the issuer that was presented,
    /// the kid that did not match.
    ///
    /// Omitted when absent, so existing audit consumers see an unchanged
    /// payload. Rejections only — a successful dispatch has nothing to
    /// explain, and this must never become a place where request content
    /// leaks into the audit log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    /// Time-to-first-event in milliseconds, for streamed (SSE) dispatches
    /// only (issue #132). `latency_ms` measures the *whole* stream to
    /// termination; this captures how quickly the first byte reached the
    /// client. Omitted on buffered dispatches so their lines stay
    /// byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
    /// The RAW platform sender id behind this principal, when the
    /// adapter knows one (see [`crate::principal::Principal::sender_ref`]).
    /// Recorded beside the resolved `subject` so an impersonation under
    /// `identity.kind: upstream` is distinguishable from the victim's own
    /// session; omitted where there is no platform sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_ref: Option<&'a str>,
    /// How many further rejections this line stands for (#249).
    ///
    /// Anonymous rejections on a public path are coalesced into one line
    /// per window per protocol, because a background scanner would
    /// otherwise write one line per probe and evict every real entry from
    /// the ring buffer. The count is what makes the coalesced line
    /// honest: the number of refusals is never lost, only the per-request
    /// repetition. `Some(0)` never appears — a line that swallowed
    /// nothing omits the field, so every pre-#249 line stays
    /// byte-identical for existing audit consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<u64>,
    pub trace_id: &'a str,
}

/// How much of a principal-derived field is recorded in an audit line.
///
/// Under `identity.kind: upstream` (FR-I-7) an out-of-process resolver
/// chooses `sub` and `tenant`, so these are attacker-influenced values
/// recorded on a path that runs BEFORE anything validates them.
/// Uncapped, a hostile resolver writes unbounded data into stdout and the
/// 1024-entry ring buffer on every request — the harm #249 addressed,
/// arriving through a different door. Observed live while verifying the
/// #250 fail-closed path: the refusal was correct, and the audit line it
/// emitted still carried the full 5000-character hostile tenant.
///
/// Generous enough that no legitimate value is ever touched: the longest
/// real subjects are GUIDs and `29:`-prefixed Teams ids.
pub const MAX_AUDITED_FIELD_LEN: usize = 128;

/// Clamp a principal-derived field for recording. Zero-copy for every
/// legitimate value; an over-long one is truncated with a marker naming
/// the true length, so an operator can still see what was refused and
/// that it was cut.
pub fn clamp_audited(value: &str) -> std::borrow::Cow<'_, str> {
    if value.len() <= MAX_AUDITED_FIELD_LEN {
        return std::borrow::Cow::Borrowed(value);
    }
    // Cut on a char boundary so the line stays valid UTF-8.
    let mut end = MAX_AUDITED_FIELD_LEN;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…[{} bytes]", &value[..end], value.len()))
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
    // Serialise the ENTRY, not the record: `From<&AuditRecord>` is the
    // one place principal-derived fields are clamped, so routing both
    // sinks through it makes the bound an invariant of the emitter
    // rather than a habit of each call site. A crew review found the
    // per-site version covered 3 of 5 sinks (#250).
    let entry = AuditEntry::from(record);
    let Ok(line) = serde_json::to_string(&entry) else {
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
    AuditBuffer::push_entry(entry);
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
    /// See [`AuditRecord::error_detail`]. Mirrored so the buffer and
    /// stdout carry the same line — and so the clamp in `From` covers
    /// it, since adapters interpolate resolver-chosen values into it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
    /// See [`AuditRecord::sender_ref`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_ref: Option<String>,
    /// See [`AuditRecord::suppressed`]. Mirrored into the buffer because
    /// the operator tailing `/v1/audit` is precisely the person who needs
    /// to know the entry stands for more than one refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<u64>,
    pub trace_id: String,
}

impl<'a> From<&AuditRecord<'a>> for AuditEntry {
    fn from(r: &AuditRecord<'a>) -> Self {
        Self {
            kind: r.kind,
            phase: r.phase,
            when: r.when.clone(),
            who: clamp_audited(r.who).into_owned(),
            what: r.what.to_string(),
            env: r.env.to_string(),
            result: r.result.clone(),
            protocol: r.protocol.to_string(),
            tool: r.tool.to_string(),
            subject: clamp_audited(r.subject).into_owned(),
            tenant: clamp_audited(r.tenant).into_owned(),
            latency_ms: r.latency_ms,
            status: r.status,
            status_label: r.status_label,
            status_detail: r.status_detail,
            // Bounded here too: adapters interpolate the resolver-chosen
            // tenant into rate-limit messages, so an unclamped
            // `error_detail` re-leaks what the fields above clamp.
            error_detail: r
                .error_detail
                .as_deref()
                .map(|d| clamp_audited(d).into_owned()),
            ttfb_ms: r.ttfb_ms,
            sender_ref: r.sender_ref.map(|v| clamp_audited(v).into_owned()),
            suppressed: r.suppressed,
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

    /// Push an already-clamped entry (see [`emit`], which builds it once
    /// and shares it between stdout and the buffer).
    fn push_entry(entry: AuditEntry) {
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
        Self::recent_where(limit, trace_id, |_| true)
    }

    /// Newest-first slice, keeping only entries `visible` accepts.
    ///
    /// The predicate is applied BEFORE `limit` (#282). Filtering after
    /// taking the window is the subtle version of the bug: a caller
    /// whose traffic is a small share of a busy gateway asks for 50 rows,
    /// gets the newest 50 across all tenants, keeps the two that are
    /// theirs, and reads it as "no activity" rather than as a paging
    /// artefact.
    ///
    /// The POLICY stays with the caller — this buffer owns recording,
    /// not who may read what. It only guarantees the ordering.
    pub fn recent_where(
        limit: usize,
        trace_id: Option<&str>,
        visible: impl Fn(&AuditEntry) -> bool,
    ) -> Vec<AuditEntry> {
        let buf = Self::global();
        let q = buf.inner.lock().unwrap_or_else(|e| e.into_inner());
        let it = q.iter().rev();
        let filtered: Box<dyn Iterator<Item = &AuditEntry>> = match trace_id {
            Some(t) => Box::new(it.filter(move |e| e.trace_id == t)),
            None => Box::new(it),
        };
        filtered
            .filter(|e| visible(e))
            .take(limit)
            .cloned()
            .collect()
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

#[cfg(test)]
mod rejection_reason_tests {
    use super::*;

    /// A refusal must say WHY in the audit line itself.
    ///
    /// `result` carries only the class (`error:auth`), which makes a
    /// wrong issuer, an unknown signing key, an untrusted reply URL and a
    /// missing header indistinguishable. Two live incidents were slow to
    /// diagnose for exactly that reason.
    #[test]
    fn a_rejection_serialises_its_reason() {
        let rec = AuditRecord {
            kind: "audit",
            phase: AuditPhase::Rejected,
            when: "2026-08-29T14:04:39Z".to_string(),
            who: "-",
            what: "msteams",
            env: "test",
            result: "error:auth".to_string(),
            protocol: "messenger:msteams",
            tool: "msteams",
            subject: "-",
            tenant: "-",
            latency_ms: 0,
            status: 401,
            status_label: None,
            status_detail: None,
            error_detail: Some("bot framework jwt: jwt issuer does not match".into()),
            ttfb_ms: None,
            sender_ref: None,
            suppressed: None,
            trace_id: "t-1",
        };
        let v = serde_json::to_value(&rec).expect("serialises");
        assert_eq!(v["result"], "error:auth");
        assert!(
            v["error_detail"].as_str().unwrap().contains("issuer"),
            "the reason must survive into the audit line: {v}"
        );
    }

    /// Absent, the field is omitted entirely — an existing audit consumer
    /// sees the payload it already saw.
    #[test]
    fn no_reason_means_no_field() {
        let rec = AuditRecord {
            kind: "audit",
            phase: AuditPhase::Dispatch,
            when: "2026-08-29T14:04:39Z".to_string(),
            who: "alice",
            what: "echo",
            env: "test",
            result: "ok".to_string(),
            protocol: "rest",
            tool: "echo",
            subject: "alice",
            tenant: "-",
            latency_ms: 3,
            status: 200,
            status_label: None,
            status_detail: None,
            error_detail: None,
            ttfb_ms: None,
            sender_ref: None,
            suppressed: None,
            trace_id: "t-2",
        };
        let v = serde_json::to_value(&rec).expect("serialises");
        assert!(v.get("error_detail").is_none(), "must be omitted: {v}");
    }
}
