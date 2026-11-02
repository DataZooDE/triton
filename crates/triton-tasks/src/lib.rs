//! Long-running tasks (ADR-20): the task store, the live-event hub
//! and the runner that drives one dispatch to its end. Shared by the HTTP
//! trio (A2A, MCP) and the Teams adapter.
//!
//! A task is one inbound turn whose answer may take longer than a caller
//! waits. The runner is a spawned tokio task that consumes the dispatcher's
//! event stream, records every state change in the [`TaskStore`] and
//! broadcasts it on the [`TaskHub`], so a blocking `message/send`, a
//! stream, a later `tasks/get` and a resubscribe all see the same task.
//!
//! When the upstream goes asynchronous (contract §7: a `task` event with
//! its own id), the runner remembers that id. If the stream then breaks —
//! the upstream closed it, the idle or total limit cut it, or Triton
//! restarted — the task is RESUMED by asking the upstream for that task
//! (`X-Triton-Task`), not re-run: a resumed call never files a second
//! request.
//!
//! The store is SQLite when `TRITON_TASK_DB` names a file (tasks survive a
//! restart) and process memory otherwise (restart-clean, G-8). Records are
//! small and bounded: reply text is clamped, the result JSON is kept only
//! under a size cap, and rows expire after the retention period.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use triton_core::{ContextOverrides, Dispatcher, Principal, StreamEvent};

/// Lifecycle of a task. The first four are what the Triton-shaped route
/// and spec 0.3 have always reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    /// Recorded before the dispatch is spawned, so a client polling
    /// immediately after `returnImmediately` sees the task.
    Submitted,
    /// The dispatch (or the upstream task) is running.
    Working,
    Completed,
    Failed,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed)
    }
}

/// The latest progress an upstream reported for a long-running task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Completion estimate, 0–100, when the upstream gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

/// One task as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// The task id: the turn's trace id.
    pub id: String,
    pub context_id: String,
    pub state: TaskState,
    /// Who may read it: [`owner_key`] of the caller that created it.
    /// `None` for records written by the legacy entry points.
    #[serde(default)]
    pub owner: Option<String>,
    /// The tool the turn was dispatched to.
    #[serde(default)]
    pub tool: String,
    /// The upstream's own task id, once it went asynchronous.
    #[serde(default)]
    pub upstream_task_id: Option<String>,
    /// The upstream's suggested polling interval, if it named one.
    #[serde(default)]
    pub check_after_seconds: Option<u64>,
    #[serde(default)]
    pub progress: Option<Progress>,
    /// The upstream's hand-over text for a caller that stops waiting
    /// ("filed, still running, the result will be in …"; mock 1k).
    #[serde(default)]
    pub receipt: Option<String>,
    /// The upstream's line under a working card's steps, if it sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The reply as text (clamped).
    #[serde(default)]
    pub result_text: Option<String>,
    /// The upstream's result JSON, kept when it is small enough.
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    /// The failure's class (`TritonError::class`, or `expired` /
    /// `interrupted` for a task the runner gave up on), so a channel shows
    /// the matching notice and audits the right class.
    #[serde(default)]
    pub error_class: Option<String>,
    /// The protocol the turn arrived on (`a2a`, `mcp`), so a resume audits
    /// and renders under the same one and A2A `ListTasks` lists A2A tasks.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// The turn's `_triton` metadata (#352), replayed on a resume (F11).
    #[serde(default)]
    pub triton_meta: Option<serde_json::Map<String, Value>>,
    /// Where the answer goes, for a channel that delivers it itself (the
    /// Teams courier: conversation reference, card activity id, the
    /// sender's identity). Opaque to the runner.
    #[serde(default)]
    pub delivery: Option<Value>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

fn default_protocol() -> String {
    "a2a".to_string()
}

impl TaskRecord {
    pub fn new(id: &str, context_id: &str, tool: &str, owner: Option<String>) -> Self {
        let now = now_ms();
        Self {
            id: id.to_string(),
            context_id: context_id.to_string(),
            state: TaskState::Submitted,
            owner,
            tool: tool.to_string(),
            upstream_task_id: None,
            check_after_seconds: None,
            progress: None,
            receipt: None,
            note: None,
            result_text: None,
            result: None,
            error: None,
            error_class: None,
            protocol: default_protocol(),
            triton_meta: None,
            delivery: None,
            created_ms: now,
            updated_ms: now,
        }
    }
}

/// The key a task's reader must match: the verified issuer (else the
/// provider name), subject and tenant. The subject alone is not unique
/// across issuers (an email from Google, a pairwise GUID from Entra, the
/// same `sub` from two generic issuers), nor across tenants (F2).
pub fn owner_key(p: &Principal) -> String {
    let issuer = p
        .idp
        .as_ref()
        .and_then(|i| i.issuer.as_deref().or(Some(i.name.as_str())))
        .unwrap_or("-");
    format!("{issuer}|{}|{}", p.sub, p.tenant)
}

/// Cap on the stored reply text — the full result already went to any
/// client that stayed connected.
const RESULT_TEXT_MAX_BYTES: usize = 32 * 1024;
/// Cap on the stored result JSON (serialized).
const RESULT_JSON_MAX_BYTES: usize = 256 * 1024;
/// In-memory store bound (FIFO eviction).
const MEMORY_CAPACITY: usize = 1024;
/// Rows a `list` scans at most.
pub const LIST_MAX: usize = 1000;

pub fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

enum Backend {
    Memory(Mutex<Memory>),
    Sqlite(Mutex<rusqlite::Connection>),
}

#[derive(Default)]
struct Memory {
    rows: HashMap<String, TaskRecord>,
    order: VecDeque<String>,
}

/// The task store. Cheap to clone; every clone is the same store.
#[derive(Clone)]
pub struct TaskStore {
    backend: Arc<Backend>,
    retention: Duration,
    /// Writes since the last prune (F5: retention is enforced at runtime).
    writes: Arc<std::sync::atomic::AtomicU64>,
    /// Serialises read-modify-writes (review F1): the runner and a channel
    /// courier (Teams: the card id) both update a running task.
    rmw: Arc<Mutex<()>>,
}

/// Prune the SQLite store every this many writes.
const PRUNE_EVERY: u64 = 256;

impl Default for TaskStore {
    fn default() -> Self {
        Self::memory()
    }
}

impl TaskStore {
    /// The historical constructor: [`Self::memory`].
    pub fn new() -> Self {
        Self::memory()
    }

    /// Process memory: bounded, restart-clean.
    pub fn memory() -> Self {
        Self {
            backend: Arc::new(Backend::Memory(Mutex::new(Memory::default()))),
            retention: Duration::from_secs(86_400),
            writes: Arc::default(),
            rmw: Arc::default(),
        }
    }

    /// A SQLite file: tasks survive a restart. The file and its table are
    /// created when missing.
    pub fn sqlite(path: &Path, retention: Duration) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| format!("open task store {}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS triton_tasks (
                 id TEXT PRIMARY KEY,
                 owner TEXT,
                 context_id TEXT NOT NULL,
                 created_ms INTEGER NOT NULL,
                 updated_ms INTEGER NOT NULL,
                 record TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS triton_tasks_owner
                 ON triton_tasks (owner, created_ms);",
        )
        .map_err(|e| format!("initialise task store {}: {e}", path.display()))?;
        let store = Self {
            backend: Arc::new(Backend::Sqlite(Mutex::new(conn))),
            retention,
            writes: Arc::default(),
            rmw: Arc::default(),
        };
        store.prune();
        Ok(store)
    }

    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.retention = retention;
        self
    }

    pub fn is_persistent(&self) -> bool {
        matches!(*self.backend, Backend::Sqlite(_))
    }

    /// A record past the retention period is gone, whether or not the
    /// prune has run yet.
    fn live(&self, rec: &TaskRecord) -> bool {
        rec.updated_ms >= now_ms() - self.retention.as_millis() as i64
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        self.get_raw(id).filter(|r| self.live(r))
    }

    fn get_raw(&self, id: &str) -> Option<TaskRecord> {
        match &*self.backend {
            Backend::Memory(m) => m.lock().unwrap().rows.get(id).cloned(),
            Backend::Sqlite(c) => {
                let c = c.lock().unwrap();
                c.query_row("SELECT record FROM triton_tasks WHERE id = ?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
            }
        }
    }

    /// Insert or replace a record. A failed write is an error the caller
    /// must handle (F10): a task that is not stored cannot be read back.
    pub fn put(&self, rec: &TaskRecord) -> Result<(), String> {
        match &*self.backend {
            Backend::Memory(m) => {
                let mut m = m.lock().unwrap();
                if !m.rows.contains_key(&rec.id) {
                    m.order.push_back(rec.id.clone());
                    // Evict the oldest FINISHED rows first; a running task
                    // goes only when nothing else is left to drop.
                    while m.order.len() > MEMORY_CAPACITY {
                        let victim = m
                            .order
                            .iter()
                            .position(|id| m.rows.get(id).is_none_or(|r| r.state.is_terminal()))
                            .unwrap_or(0);
                        if let Some(old) = m.order.remove(victim) {
                            m.rows.remove(&old);
                        }
                    }
                }
                m.rows.insert(rec.id.clone(), rec.clone());
                Ok(())
            }
            Backend::Sqlite(c) => {
                let raw = serde_json::to_string(rec).map_err(|e| e.to_string())?;
                let result = {
                    let c = c.lock().unwrap();
                    c.execute(
                    "INSERT INTO triton_tasks (id, owner, context_id, created_ms, updated_ms, record)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET updated_ms = excluded.updated_ms,
                         record = excluded.record",
                    rusqlite::params![
                        rec.id,
                        rec.owner,
                        rec.context_id,
                        rec.created_ms,
                        rec.updated_ms,
                        raw
                    ],
                )
                };
                if let Err(e) = result {
                    tracing::error!(error = %e, task = %rec.id, "task store write failed");
                    return Err(format!("task store write failed: {e}"));
                }
                let n = self
                    .writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n % PRUNE_EVERY == PRUNE_EVERY - 1 {
                    self.prune();
                }
                Ok(())
            }
        }
    }

    /// Read-modify-write one record; `None` when it does not exist.
    /// Atomic against every other `update`/`update_running` in this process
    /// (one lock across the read and the write). Several writers update a
    /// running task — the runner and a channel courier — so the lock, not
    /// a convention, keeps their writes from overwriting each other.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut TaskRecord)) -> Option<TaskRecord> {
        let _held = self.rmw.lock().unwrap();
        let mut rec = self.get(id)?;
        f(&mut rec);
        rec.updated_ms = now_ms();
        // Logged by `put`; the runner keeps going on its in-memory state.
        let _ = self.put(&rec);
        Some(rec)
    }

    /// [`Self::update`] that never touches a finished task (F3): the
    /// terminal check happens inside the critical section, so a stale
    /// reader cannot turn a completed answer back into a running task.
    pub fn update_running(&self, id: &str, f: impl FnOnce(&mut TaskRecord)) -> Option<TaskRecord> {
        let _held = self.rmw.lock().unwrap();
        let mut rec = self.get(id)?;
        if rec.state.is_terminal() {
            return None;
        }
        f(&mut rec);
        rec.updated_ms = now_ms();
        let _ = self.put(&rec);
        Some(rec)
    }

    /// The owner's tasks, newest first, optionally in one context.
    pub fn list(&self, owner: &str, context_id: Option<&str>) -> Vec<TaskRecord> {
        let mut out: Vec<TaskRecord> = match &*self.backend {
            Backend::Memory(m) => m
                .lock()
                .unwrap()
                .rows
                .values()
                .filter(|r| r.owner.as_deref() == Some(owner))
                .cloned()
                .collect(),
            Backend::Sqlite(c) => {
                let c = c.lock().unwrap();
                let Ok(mut stmt) = c.prepare(
                    "SELECT record FROM triton_tasks WHERE owner = ?1
                     ORDER BY created_ms DESC LIMIT ?2",
                ) else {
                    return Vec::new();
                };
                stmt.query_map(rusqlite::params![owner, LIST_MAX as i64], |r| {
                    r.get::<_, String>(0)
                })
                .map(|rows| {
                    rows.filter_map(Result::ok)
                        .filter_map(|raw| serde_json::from_str(&raw).ok())
                        .collect()
                })
                .unwrap_or_default()
            }
        };
        out.retain(|r| self.live(r));
        if let Some(ctx) = context_id {
            out.retain(|r| r.context_id == ctx);
        }
        out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(b.id.cmp(&a.id)));
        out.truncate(LIST_MAX);
        out
    }

    /// Rows of `protocol` matching an extra SQL condition on the record
    /// JSON, filtered in SQL (no LIMIT: this runs at boot). Errors are
    /// logged, and read as "nothing to recover".
    fn select_protocol(
        &self,
        protocol: &str,
        cond: &str,
        keep: impl Fn(&TaskRecord) -> bool,
    ) -> Vec<TaskRecord> {
        let rows: Vec<TaskRecord> = match &*self.backend {
            Backend::Memory(m) => m.lock().unwrap().rows.values().cloned().collect(),
            Backend::Sqlite(c) => {
                let c = c.lock().unwrap();
                let sql = format!(
                    "SELECT record FROM triton_tasks
                     WHERE json_extract(record, '$.protocol') = ?1 AND ({cond})
                     ORDER BY created_ms"
                );
                let mut stmt = match c.prepare(&sql) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "task store query failed");
                        return Vec::new();
                    }
                };
                match stmt.query_map([protocol], |r| r.get::<_, String>(0)) {
                    Ok(rows) => rows
                        .filter_map(|r| r.map_err(|e| tracing::error!(error = %e, "task row")).ok())
                        .filter_map(|raw| serde_json::from_str(&raw).ok())
                        .collect(),
                    Err(e) => {
                        tracing::error!(error = %e, "task store query failed");
                        Vec::new()
                    }
                }
            }
        };
        rows.into_iter()
            .filter(|r| r.protocol == protocol && self.live(r) && keep(r))
            .collect()
    }

    /// Every stored, still-running task of `protocol`.
    pub fn list_running(&self, protocol: &str) -> Vec<TaskRecord> {
        self.select_protocol(
            protocol,
            "json_extract(record, '$.state') NOT IN ('completed', 'failed')",
            |r| !r.state.is_terminal(),
        )
    }

    /// Every stored task of `protocol` whose answer a channel has not yet
    /// confirmed delivered (`delivery.delivered` not true), whatever its
    /// execution state — a crash between completion and delivery, or an
    /// expiry while the process was down, still owes the user an answer.
    pub fn list_undelivered(&self, protocol: &str) -> Vec<TaskRecord> {
        self.select_protocol(
            protocol,
            "json_extract(record, '$.delivery') IS NOT NULL
             AND COALESCE(json_extract(record, '$.delivery.delivered'), 0) = 0",
            |r| {
                r.delivery
                    .as_ref()
                    .is_some_and(|d| !d.get("delivered").and_then(Value::as_bool).unwrap_or(false))
            },
        )
    }

    /// Drop rows older than the retention period (SQLite; the memory store
    /// is bounded by count instead).
    pub fn prune(&self) {
        if let Backend::Sqlite(c) = &*self.backend {
            let cutoff = now_ms() - self.retention.as_millis() as i64;
            let _ = c
                .lock()
                .unwrap()
                .execute("DELETE FROM triton_tasks WHERE updated_ms < ?1", [cutoff]);
        }
    }

    // ---- The legacy entry points (Triton-shaped `message:send`) --------

    pub fn record(&self, trace_id: &str, state: TaskState) {
        self.record_entry(trace_id, state, None, None);
    }

    pub fn record_entry(
        &self,
        trace_id: &str,
        state: TaskState,
        result: Option<&str>,
        error: Option<&str>,
    ) {
        let mut rec = self
            .get(trace_id)
            .unwrap_or_else(|| TaskRecord::new(trace_id, trace_id, "", None));
        rec.state = state;
        if let Some(r) = result {
            rec.result_text = Some(clamp(r, RESULT_TEXT_MAX_BYTES));
        }
        if let Some(e) = error {
            rec.error = Some(clamp(e, RESULT_TEXT_MAX_BYTES));
        }
        rec.updated_ms = now_ms();
        let _ = self.put(&rec);
    }

    pub fn state(&self, trace_id: &str) -> Option<TaskState> {
        self.get(trace_id).map(|r| r.state)
    }
}

/// What a live task tells its subscribers.
#[derive(Debug, Clone)]
pub enum TaskEvent {
    /// An incremental token of the reply.
    Token(String),
    /// The upstream went asynchronous.
    Async {
        upstream_task_id: String,
        check_after_seconds: Option<u64>,
        receipt: Option<String>,
        /// The upstream's line for a working card ("you can leave this chat").
        note: Option<String>,
    },
    Progress(Progress),
    /// Terminal success with the upstream's result.
    Done(Value),
    /// Terminal failure: the error class (as stored on the record) and
    /// the raw message. Render it with [`user_text`], never verbatim.
    Failed {
        class: String,
        message: String,
    },
}

impl TaskEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskEvent::Done(_) | TaskEvent::Failed { .. })
    }
}

/// Live tasks of this process: one broadcast channel per running task.
#[derive(Clone, Default)]
pub struct TaskHub {
    live: Arc<Mutex<HashMap<String, broadcast::Sender<TaskEvent>>>>,
}

impl TaskHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a task about to run and subscribe to it, atomically, so the
    /// caller cannot miss an event. `None` when it is already running.
    fn open(
        &self,
        id: &str,
    ) -> Option<(broadcast::Sender<TaskEvent>, broadcast::Receiver<TaskEvent>)> {
        let mut live = self.live.lock().unwrap();
        if live.contains_key(id) {
            return None;
        }
        let (tx, rx) = broadcast::channel(256);
        live.insert(id.to_string(), tx.clone());
        Some((tx, rx))
    }

    /// Follow a task running in this process.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<TaskEvent>> {
        self.live.lock().unwrap().get(id).map(|tx| tx.subscribe())
    }

    /// F3: subscribe to a live task, or — under the same lock — claim a
    /// stored, still-running one for a new runner. The store is re-read
    /// under the lock, so a task that finished a moment ago is neither
    /// resumed nor failed.
    fn subscribe_or_claim(&self, store: &TaskStore, id: &str) -> Claim {
        let mut live = self.live.lock().unwrap();
        if let Some(tx) = live.get(id) {
            return Claim::Live(tx.subscribe());
        }
        match store.get(id) {
            Some(rec) if !rec.state.is_terminal() => {
                let (tx, rx) = broadcast::channel(256);
                live.insert(id.to_string(), tx.clone());
                Claim::Claimed(Box::new(rec), tx, rx)
            }
            _ => Claim::Finished,
        }
    }

    pub fn is_live(&self, id: &str) -> bool {
        self.live.lock().unwrap().contains_key(id)
    }

    fn close(&self, id: &str) {
        self.live.lock().unwrap().remove(id);
    }
}

enum Claim {
    Live(broadcast::Receiver<TaskEvent>),
    Claimed(
        Box<TaskRecord>,
        broadcast::Sender<TaskEvent>,
        broadcast::Receiver<TaskEvent>,
    ),
    Finished,
}

/// Closes a task's hub entry however the runner ends, a panic included, so
/// no dead sender is left behind (F3).
struct HubGuard {
    hub: TaskHub,
    id: String,
}

impl Drop for HubGuard {
    fn drop(&mut self) {
        self.hub.close(&self.id);
    }
}

/// When a task's record is first written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    /// At once (A2A: every turn is a task).
    Now,
    /// Only once the upstream goes asynchronous (MCP, F5): a plain call
    /// is no task and leaves nothing behind.
    OnAsync,
}

/// Timing knobs (env names in brackets are the binary's).
#[derive(Debug, Clone)]
pub struct TaskSettings {
    /// How long a blocking send waits before it returns the working task
    /// (`TRITON_A2A_BLOCK_SECS`, default 60).
    pub block: Duration,
    /// How long a client stream stays open (`TRITON_A2A_STREAM_MAX_SECS`,
    /// default 660).
    pub stream_max: Duration,
    /// How long a task may run before it is failed as expired
    /// (`TRITON_TASK_MAX_SECS`, default 3600).
    pub task_max: Duration,
    /// How long an MCP `tools/call` waits for an asynchronous upstream
    /// before it answers "filed" (`TRITON_MCP_ASYNC_WAIT_SECS`, default 20).
    pub mcp_wait: Duration,
    /// The `check_after_seconds` an MCP "filed" result suggests when the
    /// upstream named none (`TRITON_MCP_CHECK_AFTER_SECS`, default 30).
    pub mcp_check_after_secs: u64,
}

impl Default for TaskSettings {
    fn default() -> Self {
        Self {
            block: Duration::from_secs(60),
            stream_max: Duration::from_secs(660),
            task_max: Duration::from_secs(3600),
            mcp_wait: Duration::from_secs(20),
            mcp_check_after_secs: 30,
        }
    }
}

/// How the runner starts: a new dispatch, or a resume of an upstream task.
pub enum Start {
    Dispatch(Value),
    Resume(String),
}

/// Everything a runner needs. Cloned into the spawned task.
#[derive(Clone)]
pub struct Runner {
    pub dispatcher: Arc<Dispatcher>,
    pub store: TaskStore,
    pub hub: TaskHub,
    pub settings: Arc<TaskSettings>,
    /// The stored reply text of a result (the HTTP trio's `reply_text`).
    pub render: Render,
}

/// Turns an upstream result into the reply text a task stores.
pub type Render = Arc<dyn Fn(&Value) -> String + Send + Sync>;

/// A plain renderer: a surface's `text`, else the JSON.
pub fn plain_render() -> Render {
    Arc::new(|v: &Value| {
        v.pointer("/surface/text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
    })
}

/// Longest pause between two resume attempts. Resumes continue until the
/// task deadline (F6), so an upstream redeploy does not fail the task.
const RESUME_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// The failure text of a task that ran past `TRITON_TASK_MAX_SECS`.
pub const EXPIRED: &str = "the task expired before the agent finished";

/// The error class of a task that ran past its deadline.
pub const CLASS_EXPIRED: &str = "expired";
/// The error class of a task whose call was lost to a restart.
pub const CLASS_INTERRUPTED: &str = "interrupted";
/// The error class of a task the upstream itself failed with an `error`
/// event: its message is written for the caller (contract §7) and is shown
/// as it is, unlike Triton's own transport faults.
pub const CLASS_AGENT: &str = "agent";

/// The sentence a person sees for an expired task.
const EXPIRED_NOTICE: &str =
    "The request expired: it took longer than allowed and was stopped. Please send it again.";

/// The failure a stored record describes, as the dispatcher's error type:
/// the class picks the variant, the stored text is the message.
pub fn stored_error(rec: &TaskRecord) -> triton_core::TritonError {
    error_of(
        rec.error_class.as_deref().unwrap_or("tool"),
        rec.error.clone().unwrap_or_default(),
    )
}

/// What a person sees for a failed task of `class`: the fixed text of an
/// expired or interrupted task, else the class's neutral notice
/// ([`triton_core::TritonError::user_notice`]). Never the raw message,
/// which may carry an upstream's status line.
pub fn user_text(class: &str, message: &str) -> String {
    match class {
        CLASS_INTERRUPTED | CLASS_AGENT => message.to_string(),
        _ => error_notice(&error_of(class, message.to_string())).to_string(),
    }
}

/// The text a person sees for a dispatch or task error: an expired task's
/// own sentence, else the class's neutral notice.
pub fn error_notice(e: &triton_core::TritonError) -> &'static str {
    if matches!(e, triton_core::TritonError::Tool(m) if m == EXPIRED) {
        return EXPIRED_NOTICE;
    }
    e.user_notice()
}

/// [`user_text`] for a stored failed record.
pub fn failure_text(rec: &TaskRecord) -> String {
    user_text(
        rec.error_class.as_deref().unwrap_or("tool"),
        rec.error.as_deref().unwrap_or_default(),
    )
}

/// Rebuild a `TritonError` from its class. `expired` and `interrupted`
/// are tool faults whose message is the fixed task text.
pub fn error_of(class: &str, message: String) -> triton_core::TritonError {
    use triton_core::TritonError as E;
    match class {
        "auth" => E::Auth(message),
        "forbidden" => E::Forbidden(message),
        "validation" => E::Validation(message),
        "provider" => E::Provider(message),
        "ratelimit" => E::RateLimited(message),
        CLASS_EXPIRED => E::Tool(EXPIRED.to_string()),
        _ => E::Tool(message),
    }
}

/// Why [`Runner::start`] did not start a task.
#[derive(Debug)]
pub enum StartError {
    /// It is already running in this process.
    Running,
    /// Its record could not be written (F10).
    Store(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Running => f.write_str("the task is already running"),
            StartError::Store(e) => f.write_str(e),
        }
    }
}

impl Runner {
    /// Start `rec` (already shaped by the caller) and return a subscription
    /// to its events. With [`Persist::Now`] the record is stored as
    /// `working` first, and a failed write refuses the start; with
    /// [`Persist::OnAsync`] it is stored only if the upstream goes
    /// asynchronous.
    pub fn start(
        &self,
        mut rec: TaskRecord,
        start: Start,
        principal: Principal,
        protocol: &str,
        overrides: ContextOverrides,
        persist: Persist,
    ) -> Result<broadcast::Receiver<TaskEvent>, StartError> {
        let (tx, rx) = self.hub.open(&rec.id).ok_or(StartError::Running)?;
        rec.state = TaskState::Working;
        rec.protocol = protocol.to_string();
        if !overrides.triton_meta.is_empty() {
            rec.triton_meta = Some(overrides.triton_meta.clone());
        }
        rec.updated_ms = now_ms();
        if persist == Persist::Now
            && let Err(e) = self.store.put(&rec)
        {
            self.hub.close(&rec.id);
            return Err(StartError::Store(e));
        }
        self.spawn(rec, start, principal, overrides, tx, persist);
        Ok(rx)
    }

    fn spawn(
        &self,
        rec: TaskRecord,
        start: Start,
        principal: Principal,
        overrides: ContextOverrides,
        tx: broadcast::Sender<TaskEvent>,
        persist: Persist,
    ) {
        let me = self.clone();
        tokio::spawn(async move {
            let _guard = HubGuard {
                hub: me.hub.clone(),
                id: rec.id.clone(),
            };
            me.run(rec, start, principal, overrides, tx, persist).await;
        });
    }

    /// Make sure a non-terminal stored task has a runner in this process:
    /// after a restart nothing drives it until its OWNER asks (the caller
    /// checks ownership; F1). With an upstream task id it is resumed as
    /// `principal` under the turn's protocol and context; without one the
    /// call it belonged to is gone, and the task fails. Returns a
    /// subscription when the task is (now) live. Race-free against a
    /// runner that is finishing (F3).
    pub fn ensure_running(
        &self,
        rec: &TaskRecord,
        principal: &Principal,
    ) -> Option<broadcast::Receiver<TaskEvent>> {
        if rec.state.is_terminal() {
            return None;
        }
        let (rec, tx, rx) = match self.hub.subscribe_or_claim(&self.store, &rec.id) {
            Claim::Live(rx) => return Some(rx),
            Claim::Finished => return None,
            Claim::Claimed(rec, tx, rx) => (*rec, tx, rx),
        };
        let guard = HubGuard {
            hub: self.hub.clone(),
            id: rec.id.clone(),
        };
        if now_ms() - rec.created_ms > self.settings.task_max.as_millis() as i64 {
            self.fail(&rec.id, CLASS_EXPIRED, EXPIRED);
            return None;
        }
        let Some(upstream) = rec.upstream_task_id.clone() else {
            self.fail(
                &rec.id,
                CLASS_INTERRUPTED,
                "the task was interrupted by a gateway restart; send the request again",
            );
            return None;
        };
        std::mem::forget(guard); // the runner's own guard takes over
        let mut p = principal.clone();
        p.trace_id = rec.id.clone();
        let overrides = ContextOverrides {
            triton_meta: rec.triton_meta.clone().unwrap_or_default(),
            surface: None,
            // Files are never stored with a task.
            client_files: None,
        };
        self.spawn(rec, Start::Resume(upstream), p, overrides, tx, Persist::Now);
        Some(rx)
    }

    /// Fail a task that is still running; a finished one is left alone.
    pub fn fail(&self, id: &str, class: &str, msg: &str) -> Option<TaskRecord> {
        self.store.update_running(id, |r| {
            r.state = TaskState::Failed;
            r.error_class = Some(class.to_string());
            r.error = Some(clamp(msg, RESULT_TEXT_MAX_BYTES));
        })
    }

    async fn run(
        &self,
        rec: TaskRecord,
        mut start: Start,
        principal: Principal,
        mut overrides: ContextOverrides,
        tx: broadcast::Sender<TaskEvent>,
        persist: Persist,
    ) {
        use futures::StreamExt as _;
        let id = rec.id.clone();
        let tool = rec.tool.clone();
        let protocol = rec.protocol.clone();
        let mut pending = (persist == Persist::OnAsync).then(|| rec.clone());
        let mut upstream_task_id = rec.upstream_task_id.clone();
        // F6: the deadline bounds everything — opening the dispatch, every
        // wait for the next event, and the resume budget.
        let deadline = tokio::time::Instant::now()
            + self.settings.task_max.saturating_sub(Duration::from_millis(
                (now_ms() - rec.created_ms).max(0) as u64,
            ));
        let mut backoff = Duration::from_millis(250);
        // Err: (class, message).
        let outcome: Result<Value, (String, String)> = 'run: loop {
            let open = async {
                match &start {
                    Start::Dispatch(args) => {
                        self.dispatcher
                            .invoke_streaming_with(
                                &tool,
                                args.clone(),
                                principal.clone(),
                                &protocol,
                                None,
                                overrides.clone(),
                            )
                            .await
                    }
                    Start::Resume(upstream) => {
                        self.dispatcher
                            .resume_streaming_with(
                                &tool,
                                upstream,
                                principal.clone(),
                                &protocol,
                                overrides.clone(),
                            )
                            .await
                    }
                }
            };
            let opened = match tokio::time::timeout_at(deadline, open).await {
                Ok(o) => o,
                Err(_) => break 'run Err((CLASS_EXPIRED.into(), EXPIRED.into())),
            };
            // The client's files go with the first dispatch only: a resume
            // never sends them again, and the task does not hold them.
            overrides.client_files = None;
            // Why the stream ended without a terminal event, if it did.
            let mut broken_class = "tool";
            let broken: String = match opened {
                Err(e) => {
                    broken_class = e.class();
                    e.to_string()
                }
                Ok(mut events) => {
                    let mut broken = "the agent closed the stream before it finished".to_string();
                    loop {
                        let ev = match tokio::time::timeout_at(deadline, events.next()).await {
                            Err(_) => break 'run Err((CLASS_EXPIRED.into(), EXPIRED.into())),
                            Ok(None) => break,
                            Ok(Some(ev)) => ev,
                        };
                        match ev {
                            StreamEvent::Token(t) => {
                                let _ = tx.send(TaskEvent::Token(t));
                            }
                            StreamEvent::Task(v) => {
                                let Some(up) = v
                                    .get("task_id")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty() && t.len() <= 200)
                                else {
                                    continue;
                                };
                                let check = v.get("check_after_seconds").and_then(Value::as_u64);
                                let receipt = v
                                    .get("receipt")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty())
                                    .map(|t| clamp(t, 2048));
                                let note = v
                                    .get("note")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty())
                                    .map(|t| clamp(t, 512));
                                upstream_task_id = Some(up.to_string());
                                backoff = Duration::from_millis(250);
                                if let Some(mut first) = pending.take() {
                                    first.upstream_task_id = Some(up.to_string());
                                    first.check_after_seconds = check;
                                    first.receipt = receipt.clone();
                                    first.note = note.clone();
                                    first.updated_ms = now_ms();
                                    // Logged by `put`; the call goes on.
                                    let _ = self.store.put(&first);
                                } else {
                                    self.store.update_running(&id, |r| {
                                        r.upstream_task_id = Some(up.to_string());
                                        r.check_after_seconds = check;
                                        if receipt.is_some() {
                                            r.receipt = receipt.clone();
                                        }
                                        if note.is_some() {
                                            r.note = note.clone();
                                        }
                                    });
                                }
                                let _ = tx.send(TaskEvent::Async {
                                    upstream_task_id: up.to_string(),
                                    check_after_seconds: check,
                                    receipt,
                                    note,
                                });
                            }
                            StreamEvent::Progress(v) => {
                                let Some(text) = v
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty())
                                else {
                                    continue;
                                };
                                backoff = Duration::from_millis(250);
                                let progress = Progress {
                                    text: clamp(text, 2048),
                                    step: v
                                        .get("step")
                                        .and_then(Value::as_str)
                                        .map(|s| clamp(s, 128)),
                                    percent: v
                                        .get("percent")
                                        .and_then(Value::as_u64)
                                        .map(|p| p.min(100) as u8),
                                };
                                self.store
                                    .update_running(&id, |r| r.progress = Some(progress.clone()));
                                let _ = tx.send(TaskEvent::Progress(progress));
                            }
                            StreamEvent::Tool(_) => {}
                            StreamEvent::Done(v) => break 'run Ok(v),
                            StreamEvent::Error { error, detail } => {
                                // An upstream `error` frame fails the task, and
                                // so does an oversized frame; a broken or cut
                                // stream does not, when there is an upstream
                                // task to go back to.
                                let resumable = matches!(
                                    detail,
                                    Some(
                                        "upstream_stream_transport"
                                            | "upstream_stream_idle_timeout"
                                            | "upstream_stream_max_duration"
                                    )
                                );
                                if !resumable || upstream_task_id.is_none() {
                                    // No `status_detail`: the upstream's own
                                    // `error` event, meant for the caller.
                                    if detail.is_none() {
                                        break 'run Err((
                                            CLASS_AGENT.into(),
                                            error.message().to_string(),
                                        ));
                                    }
                                    break 'run Err((error.class().into(), error.to_string()));
                                }
                                broken = error.to_string();
                                break;
                            }
                        }
                    }
                    broken
                }
            };
            let Some(up) = upstream_task_id.clone() else {
                break 'run Err((broken_class.into(), broken));
            };
            if tokio::time::Instant::now() + backoff >= deadline {
                break 'run Err((
                    "tool".into(),
                    format!("the agent's task did not finish: {broken}"),
                ));
            }
            tracing::info!(task = %id, reason = %broken, "resuming the upstream task");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RESUME_BACKOFF_MAX);
            start = Start::Resume(up);
        };

        match outcome {
            Ok(value) => {
                let text = (self.render)(&value);
                let keep = serde_json::to_string(&value)
                    .map(|s| s.len() <= RESULT_JSON_MAX_BYTES)
                    .unwrap_or(false);
                self.store.update_running(&id, |r| {
                    r.state = TaskState::Completed;
                    r.result_text = Some(clamp(&text, RESULT_TEXT_MAX_BYTES));
                    r.result = keep.then(|| value.clone());
                });
                let _ = tx.send(TaskEvent::Done(value));
            }
            Err((class, e)) => {
                self.fail(&id, &class, &e);
                let _ = tx.send(TaskEvent::Failed { class, message: e });
            }
        }
    }
}

/// A task's live events as the dispatcher's stream vocabulary, for a
/// consumer written against [`StreamEvent`] (the Teams courier): tokens,
/// `task` (async), progress, then exactly one `done` or `error`. When the
/// runner ended before the subscriber saw its terminal event, the stored
/// outcome stands in.
pub fn as_stream_events(
    rx: broadcast::Receiver<TaskEvent>,
    store: TaskStore,
    id: String,
) -> futures::stream::BoxStream<'static, StreamEvent> {
    use futures::StreamExt as _;
    futures::stream::unfold((rx, false), move |(mut rx, ended)| {
        let store = store.clone();
        let id = id.clone();
        async move {
            if ended {
                return None;
            }
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        let rec = store.get(&id)?;
                        let ev = match rec.state {
                            TaskState::Completed => StreamEvent::Done(
                                rec.result.unwrap_or_else(|| {
                                    serde_json::json!({ "surface": { "text": rec.result_text.unwrap_or_default() } })
                                }),
                            ),
                            TaskState::Failed => StreamEvent::error(stored_error(&rec)),
                            _ => return None,
                        };
                        return Some((ev, (rx, true)));
                    }
                };
                let terminal = ev.is_terminal();
                let out = match ev {
                    TaskEvent::Token(t) => StreamEvent::Token(t),
                    TaskEvent::Async {
                        upstream_task_id,
                        check_after_seconds,
                        receipt,
                        note,
                    } => StreamEvent::Task(serde_json::json!({
                        "task_id": upstream_task_id,
                        "check_after_seconds": check_after_seconds,
                        "receipt": receipt,
                        "note": note,
                    })),
                    TaskEvent::Progress(p) => StreamEvent::Progress(
                        serde_json::json!({ "text": p.text, "step": p.step, "percent": p.percent }),
                    ),
                    TaskEvent::Done(v) => StreamEvent::Done(v),
                    TaskEvent::Failed { class, message } => {
                        StreamEvent::error(error_of(&class, message))
                    }
                };
                return Some((out, (rx, terminal)));
            }
        }
    })
    .boxed()
}

/// Wait on a subscription until the task ends or `limit` passes. Returns
/// the terminal event, or `None` at the limit (or when the channel closed
/// without one — the caller re-reads the store).
pub async fn wait_terminal(
    rx: &mut broadcast::Receiver<TaskEvent>,
    limit: Duration,
) -> Option<TaskEvent> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return None,
            Ok(Ok(ev)) if ev.is_terminal() => return Some(ev),
            Ok(Ok(_)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_store_round_trips_and_lists_by_owner() {
        let dir = std::env::temp_dir().join(format!("triton-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let store = TaskStore::sqlite(&path, Duration::from_secs(3600)).unwrap();
        let mut a = TaskRecord::new("t1", "c1", "tool", Some("oidc|alice|acme".into()));
        a.state = TaskState::Working;
        store.put(&a).unwrap();
        let mut b = TaskRecord::new("t2", "c2", "tool", Some("oidc|bob|acme".into()));
        b.created_ms += 1;
        store.put(&b).unwrap();
        store.update("t1", |r| {
            r.progress = Some(Progress {
                text: "Reading".into(),
                ..Progress::default()
            })
        });
        drop(store);
        // A second open of the same file sees the same tasks.
        let again = TaskStore::sqlite(&path, Duration::from_secs(3600)).unwrap();
        let t1 = again.get("t1").expect("persisted");
        assert_eq!(t1.state, TaskState::Working);
        assert_eq!(t1.progress.unwrap().text, "Reading");
        assert_eq!(again.list("oidc|alice|acme", None).len(), 1);
        assert_eq!(again.list("oidc|alice|acme", Some("c2")).len(), 0);
        assert!(again.get("nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn runner(store: TaskStore) -> Runner {
        Runner {
            dispatcher: Arc::new(Dispatcher::new(
                Arc::new(triton_core::ToolRegistry::new()),
                "test".to_string(),
                triton_core::dispatcher::DispatchControls::unenforced(),
            )),
            store,
            hub: TaskHub::new(),
            settings: Arc::new(TaskSettings::default()),
            render: plain_render(),
        }
    }

    fn principal(sub: &str, issuer: &str) -> Principal {
        Principal {
            sub: sub.into(),
            scopes: Vec::new(),
            groups: Vec::new(),
            tenant: "-".into(),
            raw_token: String::new(),
            trace_id: "t".into(),
            sender_ref: None,
            conversation_ref: None,
            idp: Some(triton_core::IdpIdentity::new("oidc").issuer(issuer)),
        }
    }

    /// F2: two generic issuers minting the same `sub` are two owners.
    #[test]
    fn owner_keys_differ_by_issuer() {
        let a = owner_key(&principal("alice", "https://a.example"));
        let b = owner_key(&principal("alice", "https://b.example"));
        assert_ne!(a, b);
        assert_eq!(a, owner_key(&principal("alice", "https://a.example")));
    }

    /// F3: a reader holding a stale `working` snapshot of a task that has
    /// just completed (its runner gone) must neither fail it nor resume it.
    #[tokio::test]
    async fn a_stale_snapshot_does_not_overwrite_a_completed_task() {
        let store = TaskStore::memory();
        let r = runner(store.clone());
        let mut stale = TaskRecord::new("t1", "c1", "tool", Some("k".into()));
        stale.state = TaskState::Working;
        let mut done = stale.clone();
        done.state = TaskState::Completed;
        done.result_text = Some("the answer".into());
        store.put(&done).unwrap();
        assert!(r.ensure_running(&stale, &principal("alice", "x")).is_none());
        let after = store.get("t1").unwrap();
        assert_eq!(after.state, TaskState::Completed, "{after:?}");
        assert_eq!(after.result_text.as_deref(), Some("the answer"));
        // And with an upstream id: no resume either.
        let mut stale2 = stale.clone();
        stale2.id = "t2".into();
        stale2.upstream_task_id = Some("up".into());
        let mut done2 = stale2.clone();
        done2.state = TaskState::Completed;
        store.put(&done2).unwrap();
        assert!(
            r.ensure_running(&stale2, &principal("alice", "x"))
                .is_none()
        );
        assert!(!r.hub.is_live("t2"), "no runner was started");
    }

    /// F10: a task whose record cannot be written is not started.
    #[tokio::test]
    async fn a_failed_store_write_refuses_the_start() {
        let dir = std::env::temp_dir().join(format!("triton-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let store = TaskStore::sqlite(&path, Duration::from_secs(3600)).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE triton_tasks;")
            .unwrap();
        let r = runner(store);
        let started = r.start(
            TaskRecord::new("t1", "c1", "tool", None),
            Start::Dispatch(serde_json::json!({})),
            principal("alice", "x"),
            "a2a",
            ContextOverrides::default(),
            Persist::Now,
        );
        assert!(matches!(started, Err(StartError::Store(_))));
        assert!(!r.hub.is_live("t1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review F1: concurrent read-modify-writes of different fields (the
    /// runner's progress, the courier's card id) both persist, and a
    /// finished task is never reverted by a writer holding a stale copy.
    fn concurrent_updates(store: TaskStore) {
        let mut rec = TaskRecord::new("t1", "c1", "tool", None);
        rec.state = TaskState::Working;
        rec.delivery = Some(serde_json::json!({}));
        store.put(&rec).unwrap();
        let a = {
            let s = store.clone();
            std::thread::spawn(move || {
                for i in 0..300 {
                    s.update_running("t1", |r| {
                        r.progress = Some(Progress {
                            text: format!("step {i}"),
                            ..Progress::default()
                        })
                    });
                }
            })
        };
        let b = {
            let s = store.clone();
            std::thread::spawn(move || {
                for i in 0..300 {
                    s.update_running("t1", |r| {
                        r.delivery.as_mut().unwrap()[format!("k{i}")] = serde_json::json!(i);
                    });
                }
            })
        };
        a.join().unwrap();
        b.join().unwrap();
        let after = store.get("t1").unwrap();
        let d = after.delivery.unwrap();
        assert_eq!(
            d.as_object().unwrap().len(),
            300,
            "no card-side write was lost"
        );
        assert_eq!(
            after.progress.unwrap().text,
            "step 299",
            "no runner-side write was lost"
        );

        // A stale writer racing the terminal write never reverts it.
        let c = {
            let s = store.clone();
            std::thread::spawn(move || {
                for _ in 0..300 {
                    s.update_running("t1", |r| r.receipt = Some("x".into()));
                }
            })
        };
        store.update_running("t1", |r| r.state = TaskState::Completed);
        c.join().unwrap();
        assert_eq!(store.get("t1").unwrap().state, TaskState::Completed);
    }

    #[test]
    fn concurrent_updates_are_atomic_in_memory() {
        concurrent_updates(TaskStore::memory());
    }

    #[test]
    fn concurrent_updates_are_atomic_in_sqlite() {
        let dir = std::env::temp_dir().join(format!("triton-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TaskStore::sqlite(&dir.join("t.db"), Duration::from_secs(3600)).unwrap();
        concurrent_updates(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review F7: the boot listing filters in SQL — an old running task of
    /// one protocol is found behind any number of newer rows of others.
    #[test]
    fn listing_filters_in_sql_without_a_window() {
        let dir = std::env::temp_dir().join(format!("triton-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TaskStore::sqlite(&dir.join("t.db"), Duration::from_secs(3600)).unwrap();
        let mut old = TaskRecord::new("old", "c", "tool", None);
        old.protocol = "messenger:msteams".into();
        old.state = TaskState::Working;
        old.delivery = Some(serde_json::json!({ "delivered": false }));
        old.created_ms -= 1000;
        store.put(&old).unwrap();
        for i in 0..(LIST_MAX * 10 + 5) {
            let mut r = TaskRecord::new(&format!("n{i}"), "c", "tool", None);
            r.state = TaskState::Working;
            store.put(&r).unwrap();
        }
        let mut done = TaskRecord::new("done", "c", "tool", None);
        done.protocol = "messenger:msteams".into();
        done.state = TaskState::Completed;
        done.delivery = Some(serde_json::json!({ "delivered": true }));
        store.put(&done).unwrap();
        let running: Vec<String> = store
            .list_running("messenger:msteams")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(running, vec!["old".to_string()]);
        let undelivered: Vec<String> = store
            .list_undelivered("messenger:msteams")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(undelivered, vec!["old".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_memory_store_is_bounded() {
        let store = TaskStore::memory();
        for i in 0..(MEMORY_CAPACITY + 10) {
            store.record(&format!("t{i}"), TaskState::Completed);
        }
        assert!(store.get("t0").is_none(), "the oldest scrolled off");
        assert!(store.get(&format!("t{}", MEMORY_CAPACITY + 9)).is_some());
    }
}
