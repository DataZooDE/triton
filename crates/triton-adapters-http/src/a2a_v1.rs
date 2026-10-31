//! A2A protocol **1.0** beside 0.3 on the same endpoint (ADR-19).
//!
//! `a2a_spec` keeps the 0.3 JSON-RPC face Gemini Enterprise and Copilot
//! Studio registered against; this module adds what 1.0 needs and what
//! both versions share:
//!
//! * **Version selection** — `A2A-Version` header or query parameter:
//!   absent or `0.3[.n]` → 0.3, `1.0[.n]` → 1.0, anything else →
//!   VersionNotSupported (`-32009`), answered in the form of the version
//!   the method name belongs to (`message/send` vs `SendMessage`).
//! * **1.0 JSON-RPC** — `SendMessage`, `SendStreamingMessage`, `GetTask`,
//!   `ListTasks`, `CancelTask`, `SubscribeToTask`; push configs → `-32003`,
//!   `GetExtendedAgentCard` → `-32004`.
//! * **1.0 HTTP+JSON** under `<endpoint>/v1`: `POST /message:send`,
//!   `POST /message:stream`, `GET /tasks`, `GET /tasks/{id}`,
//!   `POST /tasks/{id}:cancel`, `GET|POST /tasks/{id}:subscribe`. 1.0 only.
//! * **Both cards** — 1.0 (`supportedInterfaces`, typed security schemes)
//!   and 0.3 (`url` + `preferredTransport`, OpenAPI-style security).
//! * **Task rendering** — one stored [`TaskRecord`] in either version.
//!
//! Semantics and error forms are ported from a portfolio agent's A2A adapter
//! (`a2a.rs`/`a2a/v03.rs`): no multi-turn (a message naming a `taskId` is
//! refused), no cancel, no push notifications, no extended card.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::a2a::A2aState;
use crate::a2a_spec::{CardState, SpecA2aConfig, SpecState};
use crate::tasks::{TaskEvent, TaskRecord, TaskState, owner_key, wait_terminal};

pub(crate) const V10: &str = "1.0";
const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const A2A_DOMAIN: &str = "a2a-protocol.org";
const REST_CONTENT_TYPE: &str = "application/a2a+json";
const ID_MAX: usize = 200;
const CONTEXT_ID_MAX: usize = 128;
const PAGE_SIZE_DEFAULT: usize = 20;
const PAGE_SIZE_MAX: usize = 100;

// ---------------------------------------------------------------------------
// Version selection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    V03,
    V10,
}

/// `""` (0.3: 1.0 spec §3.6.2, 0.3 clients send none), `0.3`, `1.0`, each
/// with an optional numeric patch level.
pub(crate) fn parse_version(raw: &str) -> Option<Version> {
    let v = raw.trim();
    if v.is_empty() {
        return Some(Version::V03);
    }
    [("0.3", Version::V03), (V10, Version::V10)]
        .into_iter()
        .find_map(|(prefix, selected)| {
            let rest = v.strip_prefix(prefix)?;
            let ok = rest.is_empty()
                || rest
                    .strip_prefix('.')
                    .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
            ok.then_some(selected)
        })
}

/// The version a request speaks: the `A2A-Version` header, else the query
/// parameter of that name. `Err` carries the refused value (sanitised).
pub(crate) fn requested_version(
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Version, String> {
    let from_query = query.and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k.eq_ignore_ascii_case("a2a-version"))
            .map(|(_, v)| v.into_owned())
    });
    let raw = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or(from_query)
        .unwrap_or_default();
    parse_version(&raw).ok_or_else(|| {
        raw.trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(20)
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum A2aError {
    Parse,
    MethodNotFound(String),
    InvalidParams(String),
    TaskNotFound,
    TaskNotCancelable,
    PushNotSupported,
    Unsupported(String),
    /// 0.3 `agent/getAuthenticatedExtendedCard`.
    ExtendedCardNotConfigured,
    VersionNotSupported(String),
    Unauthenticated(String),
    /// Authenticated, but the token lacks the delegated scope (ADR-22).
    PermissionDenied(String),
    Internal(String),
}

impl A2aError {
    pub(crate) fn code(&self) -> i64 {
        match self {
            A2aError::Parse => -32700,
            A2aError::Unauthenticated(_) | A2aError::PermissionDenied(_) => -32600,
            A2aError::MethodNotFound(_) => -32601,
            A2aError::InvalidParams(_) => -32602,
            A2aError::Internal(_) => -32603,
            A2aError::TaskNotFound => -32001,
            A2aError::TaskNotCancelable => -32002,
            A2aError::PushNotSupported => -32003,
            A2aError::Unsupported(_) => -32004,
            A2aError::ExtendedCardNotConfigured => -32007,
            A2aError::VersionNotSupported(_) => -32009,
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            A2aError::TaskNotFound | A2aError::MethodNotFound(_) => StatusCode::NOT_FOUND,
            A2aError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            A2aError::PermissionDenied(_) => StatusCode::FORBIDDEN,
            A2aError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    fn reason(&self) -> Option<&'static str> {
        Some(match self {
            A2aError::TaskNotFound => "TASK_NOT_FOUND",
            A2aError::TaskNotCancelable => "TASK_NOT_CANCELABLE",
            A2aError::PushNotSupported => "PUSH_NOTIFICATION_NOT_SUPPORTED",
            A2aError::Unsupported(_) => "UNSUPPORTED_OPERATION",
            A2aError::ExtendedCardNotConfigured => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            A2aError::VersionNotSupported(_) => "VERSION_NOT_SUPPORTED",
            _ => return None,
        })
    }

    pub(crate) fn message(&self) -> String {
        match self {
            A2aError::Parse => "Invalid JSON payload".into(),
            A2aError::InvalidParams(m)
            | A2aError::Unsupported(m)
            | A2aError::Unauthenticated(m)
            | A2aError::PermissionDenied(m)
            | A2aError::Internal(m)
            | A2aError::MethodNotFound(m) => m.clone(),
            A2aError::TaskNotFound => "The task does not exist or was not created by you".into(),
            A2aError::TaskNotCancelable => {
                "Tasks of this agent cannot be canceled: the agent may have acted on the \
                 request already"
                    .into()
            }
            A2aError::PushNotSupported => {
                "This agent does not send push notifications; poll the task or stream it".into()
            }
            A2aError::ExtendedCardNotConfigured => {
                "This agent has no extended card; the public card is complete".into()
            }
            A2aError::VersionNotSupported(v) => format!(
                "A2A protocol version {} is not supported; this agent speaks 1.0 over \
                 JSON-RPC and HTTP+JSON and 0.3 over JSON-RPC (A2A-Version: 1.0, or none for 0.3)",
                if v.is_empty() { "(none)" } else { v }
            ),
        }
    }

    /// 1.0: `data` is a list of `google.rpc.ErrorInfo`.
    fn details_v10(&self) -> Vec<Value> {
        let Some(reason) = self.reason() else {
            return Vec::new();
        };
        let mut info = json!({ "@type": ERROR_INFO, "reason": reason, "domain": A2A_DOMAIN });
        if let A2aError::VersionNotSupported(v) = self {
            info["metadata"] = json!({ "requestedVersion": v, "supportedVersions": "0.3,1.0" });
        }
        vec![info]
    }

    /// 0.3: a plain JSON-RPC error object; `data` is a plain object.
    pub(crate) fn body_v03(&self) -> Value {
        let mut body = json!({ "code": self.code(), "message": self.message() });
        if let A2aError::VersionNotSupported(v) = self {
            body["data"] = json!({ "requestedVersion": v, "supportedVersions": ["0.3", "1.0"] });
        }
        body
    }

    fn body_v10(&self) -> Value {
        let mut body = json!({ "code": self.code(), "message": self.message() });
        let details = self.details_v10();
        if !details.is_empty() {
            body["data"] = json!(details);
        }
        body
    }
}

fn transport_status(e: &A2aError) -> StatusCode {
    // A JSON-RPC error is a 200 with an `error` member; only a missing
    // credential keeps its HTTP status, which a client's middleware acts on.
    if matches!(
        e,
        A2aError::Unauthenticated(_) | A2aError::PermissionDenied(_)
    ) {
        e.status()
    } else {
        StatusCode::OK
    }
}

pub(crate) fn rpc_error_v10(id: &Value, e: &A2aError) -> Response {
    (
        transport_status(e),
        axum::Json(json!({ "jsonrpc": "2.0", "id": id, "error": e.body_v10() })),
    )
        .into_response()
}

pub(crate) fn rpc_error_v03(id: &Value, e: &A2aError) -> Response {
    (
        transport_status(e),
        axum::Json(json!({ "jsonrpc": "2.0", "id": id, "error": e.body_v03() })),
    )
        .into_response()
}

fn rest_status_name(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::INTERNAL_SERVER_ERROR => "INTERNAL",
        _ => "FAILED_PRECONDITION",
    }
}

fn rest_error(e: &A2aError) -> Response {
    let status = e.status();
    let body = json!({ "error": {
        "code": status.as_u16(),
        "status": rest_status_name(status),
        "message": e.message(),
        "details": e.details_v10(),
    } });
    (
        status,
        [(header::CONTENT_TYPE, REST_CONTENT_TYPE)],
        body.to_string(),
    )
        .into_response()
}

fn rest_ok(v: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, REST_CONTENT_TYPE)],
        v.to_string(),
    )
        .into_response()
}

fn rpc_ok(id: &Value, result: Value) -> Response {
    axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

// ---------------------------------------------------------------------------
// Rendering a stored task
// ---------------------------------------------------------------------------

/// RFC 3339 UTC timestamp of unix milliseconds (`2026-10-31T10:00:00.123Z`).
pub(crate) fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Parse an RFC 3339 timestamp (`2026-10-31T10:00:00Z`, fractional
/// seconds and `±hh:mm` offsets allowed) to unix milliseconds (F12).
pub(crate) fn parse_rfc3339(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (date, rest) = s.split_once(['T', 't'])?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let (time, offset_ms) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        let (oh, om): (i64, i64) = (oh.parse().ok()?, om.parse().ok()?);
        if oh > 23 || om > 59 {
            return None;
        }
        (t, sign * (oh * 3600 + om * 60) * 1000)
    };
    let (hms, frac) = match time.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time, ""),
    };
    let mut t = hms.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if t.next().is_some() || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days from civil (H. Hinnant).
    let (yy, mm2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000 + millis - offset_ms)
}

fn state_v10(s: TaskState) -> &'static str {
    match s {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
    }
}

fn state_v03(s: TaskState) -> &'static str {
    match s {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
    }
}

/// The text a task's status carries: the error when failed, the latest
/// progress while working.
fn status_text(rec: &TaskRecord) -> Option<String> {
    match rec.state {
        // Never the raw error: it may carry the upstream's status line.
        TaskState::Failed => Some(crate::tasks::failure_text(rec)),
        TaskState::Working | TaskState::Submitted => rec
            .progress
            .as_ref()
            .map(|p| p.text.clone())
            .or_else(|| rec.receipt.clone()),
        TaskState::Completed => None,
    }
}

fn agent_message_v10(task: &str, ctx: &str, suffix: &str, text: &str) -> Value {
    json!({ "messageId": format!("{task}:{suffix}"), "contextId": ctx, "taskId": task,
        "role": "ROLE_AGENT", "parts": [{ "text": text }] })
}

fn agent_message_v03(task: &str, ctx: &str, suffix: &str, text: &str) -> Value {
    json!({ "kind": "message", "messageId": format!("{task}:{suffix}"), "contextId": ctx,
        "taskId": task, "role": "agent", "parts": [{ "kind": "text", "text": text }] })
}

fn status_v10(rec: &TaskRecord) -> Value {
    let mut status = json!({ "state": state_v10(rec.state), "timestamp": rfc3339(rec.updated_ms) });
    if let Some(text) = status_text(rec) {
        status["message"] = agent_message_v10(&rec.id, &rec.context_id, "status", &text);
    }
    status
}

fn status_v03(rec: &TaskRecord) -> Value {
    let mut status = json!({ "state": state_v03(rec.state), "timestamp": rfc3339(rec.updated_ms) });
    if let Some(text) = status_text(rec) {
        status["message"] = agent_message_v03(&rec.id, &rec.context_id, "status", &text);
    }
    status
}

/// A blocking call that reached its cap hands the task over (mock 1k):
/// the status message is the upstream's receipt — "filed, still running,
/// where the result will be" — when it sent one, not the latest step.
pub(crate) fn handover(mut rec: TaskRecord) -> TaskRecord {
    if !rec.state.is_terminal()
        && let Some(receipt) = rec.receipt.clone()
    {
        rec.progress = Some(crate::tasks::Progress {
            text: receipt,
            ..rec.progress.unwrap_or_default()
        });
    }
    rec
}

/// A2UI v0.9 messages for a stored result, when the caller activated the
/// extension.
fn a2ui_messages(rec: &TaskRecord, a2ui: bool) -> Option<Vec<Value>> {
    if !a2ui {
        return None;
    }
    rec.result
        .as_ref()
        .and_then(triton_core::a2ui::ge::build_messages)
}

fn artifact_v10(rec: &TaskRecord, a2ui: bool) -> Option<Value> {
    if rec.state != TaskState::Completed {
        return None;
    }
    let msgs = a2ui_messages(rec, a2ui);
    let text = match (&msgs, rec.result.as_ref()) {
        (Some(_), Some(result)) => crate::a2a_spec::text_part(result, true),
        _ => rec.result_text.clone().unwrap_or_default(),
    };
    let mut parts = vec![json!({ "text": text })];
    if let Some(msgs) = msgs {
        parts.extend(
            msgs.into_iter()
                .map(|m| json!({ "data": m, "mediaType": triton_core::a2ui::ge::MIME })),
        );
    }
    Some(json!({ "artifactId": format!("{}:answer", rec.id), "name": "Answer", "parts": parts }))
}

fn artifact_v03(rec: &TaskRecord) -> Option<Value> {
    if rec.state != TaskState::Completed {
        return None;
    }
    Some(
        json!({ "artifactId": format!("{}:answer", rec.id), "name": "Answer",
        "parts": [{ "kind": "text", "text": rec.result_text.clone().unwrap_or_default() }] }),
    )
}

fn metadata(rec: &TaskRecord) -> Option<Value> {
    let mut m = serde_json::Map::new();
    if let Some(step) = rec.progress.as_ref().and_then(|p| p.step.clone())
        && !rec.state.is_terminal()
    {
        m.insert("step".into(), json!(step));
    }
    if let Some(pct) = rec.progress.as_ref().and_then(|p| p.percent)
        && !rec.state.is_terminal()
    {
        m.insert("percent".into(), json!(pct));
    }
    if let Some(secs) = rec.check_after_seconds
        && !rec.state.is_terminal()
    {
        m.insert("checkAfterSeconds".into(), json!(secs));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

pub(crate) fn task_v10(rec: &TaskRecord, with_artifacts: bool, a2ui: bool) -> Value {
    let mut t = json!({ "id": rec.id, "contextId": rec.context_id, "status": status_v10(rec) });
    if with_artifacts {
        t["artifacts"] = json!(artifact_v10(rec, a2ui).into_iter().collect::<Vec<_>>());
    }
    if let Some(m) = metadata(rec) {
        t["metadata"] = m;
    }
    t
}

pub(crate) fn task_v03(rec: &TaskRecord) -> Value {
    let mut t = json!({ "kind": "task", "id": rec.id, "contextId": rec.context_id,
        "status": status_v03(rec) });
    if let Some(a) = artifact_v03(rec) {
        t["artifacts"] = json!([a]);
    }
    if let Some(m) = metadata(rec) {
        t["metadata"] = m;
    }
    t
}

// ---------------------------------------------------------------------------
// Access
// ---------------------------------------------------------------------------

/// How a reader may use a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// The task's owner: read, subscribe, and resume it.
    Owner,
    /// A named audit operator: read only — never resumes it under the
    /// operator's credentials (F1).
    Operator,
    /// A record without an owner (the Triton-shaped route): readable by
    /// the `/v1/trace` rule, never resumed.
    Legacy,
}

/// The stored task, if `principal` may read it (F1). An owned task is its
/// owner's — a colleague in the same data tenant (an Entra tenant is a
/// whole organisation) does not see it; a named audit operator reads it.
/// Only records without an owner fall back to the `/v1/trace` rule.
/// Anything else is "not found": ids must stay unenumerable.
pub(crate) fn readable(
    a2a: &A2aState,
    principal: &triton_core::Principal,
    id: &str,
) -> Result<(TaskRecord, Access), A2aError> {
    if id.is_empty() || id.len() > ID_MAX {
        return Err(A2aError::TaskNotFound);
    }
    let rec = a2a.tasks.get(id).ok_or(A2aError::TaskNotFound)?;
    let access = match rec.owner.as_deref() {
        Some(owner) if owner == owner_key(principal) => Access::Owner,
        Some(_) if crate::rest::is_named_operator(principal, &a2a.audit_operators, a2a.env()) => {
            Access::Operator
        }
        Some(_) => return Err(A2aError::TaskNotFound),
        None if crate::rest::may_read_trace(principal, &a2a.audit_operators, a2a.env(), id) => {
            Access::Legacy
        }
        None => return Err(A2aError::TaskNotFound),
    };
    a2a.dispatcher
        .deny_if_revoked(principal, "tasks/get", "a2a")
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    Ok((rec, access))
}

/// [`readable`], and — for the owner of a task left working by a previous
/// process — a runner resumed as the owner. Everyone else gets the stored
/// snapshot and no subscription.
pub(crate) fn readable_live(
    a2a: &A2aState,
    principal: &triton_core::Principal,
    id: &str,
) -> Result<(TaskRecord, Option<broadcast::Receiver<TaskEvent>>), A2aError> {
    let (rec, access) = readable(a2a, principal, id)?;
    let rx = match access {
        Access::Owner => a2a.runner().ensure_running(&rec, principal),
        Access::Operator | Access::Legacy => None,
    };
    let rec = a2a.tasks.get(id).unwrap_or(rec);
    Ok((rec, rx))
}

// ---------------------------------------------------------------------------
// The shared operations
// ---------------------------------------------------------------------------

fn field<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| v.get(snake))
}

fn opt_str(v: &Value, camel: &str, snake: &str) -> Option<String> {
    field(v, camel, snake)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn valid_context_id(id: &str) -> bool {
    id.len() <= CONTEXT_ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

struct Incoming {
    text: String,
    context_id: Option<String>,
    task_id: Option<String>,
    return_immediately: bool,
}

/// A 1.0 `SendMessageRequest`, validated.
fn parse_send(params: &Value) -> Result<Incoming, A2aError> {
    let bad = |m: &str| A2aError::InvalidParams(m.to_string());
    let msg = params
        .get("message")
        .filter(|m| m.is_object())
        .ok_or_else(|| bad("message is required"))?;
    opt_str(msg, "messageId", "message_id")
        .filter(|id| id.chars().count() <= ID_MAX)
        .ok_or_else(|| bad("message.messageId is required (at most 200 characters)"))?;
    let role_ok = match msg.get("role") {
        Some(Value::String(r)) => r == "ROLE_USER" || r == "user",
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    };
    if !role_ok {
        return Err(bad("message.role must be ROLE_USER"));
    }
    let context_id = opt_str(msg, "contextId", "context_id");
    if context_id.as_deref().is_some_and(|c| !valid_context_id(c)) {
        return Err(bad(
            "message.contextId must have at most 128 characters from A-Z a-z 0-9 - _ . :",
        ));
    }
    let parts = msg
        .get("parts")
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| bad("message.parts needs at least one part"))?;
    // An A2UI click (a signed `sourceComponentId` in a data part) names
    // the complete request — the same resolution as 0.3, so the upstream
    // never sees the host's "User action triggered." placeholder. A source
    // button (open a document) has no 1.0 document reply; its label is
    // dispatched as the turn.
    let action = match crate::a2a_spec::a2ui_action(parts) {
        Some(triton_core::a2ui::ge::GeAction::Message(text)) => Some(text),
        Some(triton_core::a2ui::ge::GeAction::OpenDoc { label, .. }) => Some(label),
        // A form submit without its required inputs sends nothing.
        Some(triton_core::a2ui::ge::GeAction::Incomplete(names)) => {
            return Err(bad(&format!("missing input(s): {}", names.join(", "))));
        }
        None => None,
    };
    let text = action
        .unwrap_or_else(|| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(bad("the message needs a text part"));
    }
    let return_immediately = params
        .get("configuration")
        .and_then(|c| field(c, "returnImmediately", "return_immediately"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Incoming {
        text,
        context_id,
        task_id: opt_str(msg, "taskId", "task_id"),
        return_immediately,
    })
}

/// Start a turn as a task. Shared by both versions' send and stream.
pub(crate) fn start_task(
    state: &SpecState,
    principal: &triton_core::Principal,
    text: &str,
    context_id: &str,
    overrides: triton_core::ContextOverrides,
) -> Result<(TaskRecord, broadcast::Receiver<TaskEvent>), A2aError> {
    let tool = state.config.default_tool.clone();
    let rec = TaskRecord::new(
        &principal.trace_id,
        context_id,
        &tool,
        Some(owner_key(principal)),
    );
    let rx = state
        .a2a
        .runner()
        .start(
            rec.clone(),
            crate::tasks::Start::Dispatch(json!({ "message": text })),
            principal.clone(),
            "a2a",
            overrides,
            crate::tasks::Persist::Now,
        )
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    Ok((state.a2a.tasks.get(&rec.id).unwrap_or(rec), rx))
}

/// No multi-turn: a message that names a task is refused.
fn refuse_continuation(
    a2a: &A2aState,
    principal: &triton_core::Principal,
    task_id: &str,
) -> A2aError {
    match readable(a2a, principal, task_id).map(|(r, _)| r) {
        Err(e) => e,
        Ok(rec) if rec.state.is_terminal() => A2aError::Unsupported(
            "The task is finished and takes no further message; send a new message without taskId"
                .into(),
        ),
        Ok(_) => A2aError::Unsupported(
            "This agent does not continue tasks; send a new, complete message without taskId"
                .into(),
        ),
    }
}

async fn op_send(
    state: &SpecState,
    principal: triton_core::Principal,
    params: &Value,
    a2ui: bool,
) -> Result<Value, A2aError> {
    let incoming = parse_send(params)?;
    if let Some(task_id) = &incoming.task_id {
        return Err(refuse_continuation(&state.a2a, &principal, task_id));
    }
    let context_id = incoming
        .context_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let overrides = crate::a2a_spec::turn_context_for(state, &context_id, a2ui);
    let (rec, mut rx) = start_task(state, &principal, &incoming.text, &context_id, overrides)?;
    if !incoming.return_immediately {
        // Blocking is the default; capped so a platform's HTTP timeout is
        // not reached. At the cap the working task comes back.
        wait_terminal(&mut rx, state.a2a.task_settings.block).await;
    }
    let rec = handover(state.a2a.tasks.get(&rec.id).unwrap_or(rec));
    Ok(json!({ "task": task_v10(&rec, true, a2ui) }))
}

fn op_get(
    state: &SpecState,
    principal: &triton_core::Principal,
    params: &Value,
    a2ui: bool,
) -> Result<Value, A2aError> {
    let id = opt_str(params, "id", "id")
        .ok_or_else(|| A2aError::InvalidParams("id is required".into()))?;
    let (rec, _) = readable_live(&state.a2a, principal, &id)?;
    Ok(task_v10(&rec, true, a2ui))
}

fn op_cancel(state: &SpecState, principal: &triton_core::Principal, params: &Value) -> A2aError {
    match opt_str(params, "id", "id") {
        None => A2aError::InvalidParams("id is required".into()),
        Some(id) => match readable(&state.a2a, principal, &id).map(|(r, _)| r) {
            Ok(_) => A2aError::TaskNotCancelable,
            Err(e) => e,
        },
    }
}

/// Values in a REST query arrive as strings, in JSON-RPC as typed values.
fn loose(v: &Value, camel: &str, snake: &str) -> Option<String> {
    field(v, camel, snake)
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

fn encode_cursor(offset: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("ListTasks:{offset}"))
}

fn decode_cursor(token: &str) -> Option<usize> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()?;
    String::from_utf8(raw)
        .ok()?
        .strip_prefix("ListTasks:")?
        .parse()
        .ok()
}

fn op_list(
    state: &SpecState,
    principal: &triton_core::Principal,
    params: &Value,
    a2ui: bool,
) -> Result<Value, A2aError> {
    let bad = |m: &str| A2aError::InvalidParams(m.to_string());
    let page_size = match loose(params, "pageSize", "page_size") {
        None => PAGE_SIZE_DEFAULT,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if (1..=PAGE_SIZE_MAX).contains(&n) => n,
            _ => return Err(bad("pageSize must be from 1 to 100")),
        },
    };
    let offset = match loose(params, "pageToken", "page_token") {
        None => 0,
        Some(t) => decode_cursor(&t).ok_or_else(|| bad("pageToken is not a token of ListTasks"))?,
    };
    let status =
        loose(params, "status", "status").filter(|s| s != "TASK_STATE_UNSPECIFIED" && s != "0");
    if let Some(s) = &status
        && ![
            "TASK_STATE_SUBMITTED",
            "TASK_STATE_WORKING",
            "TASK_STATE_COMPLETED",
            "TASK_STATE_FAILED",
            "TASK_STATE_CANCELED",
            "TASK_STATE_INPUT_REQUIRED",
            "TASK_STATE_REJECTED",
            "TASK_STATE_AUTH_REQUIRED",
        ]
        .contains(&s.as_str())
    {
        return Err(bad("status is not a task state"));
    }
    let include_artifacts = match loose(params, "includeArtifacts", "include_artifacts").as_deref()
    {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(bad("includeArtifacts must be true or false")),
    };
    let after = match loose(params, "statusTimestampAfter", "status_timestamp_after") {
        None => None,
        Some(raw) => Some(
            parse_rfc3339(&raw)
                .ok_or_else(|| bad("statusTimestampAfter must be an RFC 3339 timestamp"))?,
        ),
    };
    let context = loose(params, "contextId", "context_id");
    state
        .a2a
        .dispatcher
        .deny_if_revoked(principal, "tasks/list", "a2a")
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    let all: Vec<TaskRecord> = state
        .a2a
        .tasks
        .list(&owner_key(principal), context.as_deref())
        .into_iter()
        // A2A lists A2A turns; an MCP call filed as a task is not one.
        .filter(|r| r.protocol == "a2a")
        .filter(|r| status.as_deref().is_none_or(|s| s == state_v10(r.state)))
        .filter(|r| after.is_none_or(|a| r.updated_ms > a))
        .collect();
    let total = all.len();
    let page: Vec<Value> = all
        .iter()
        .skip(offset)
        .take(page_size)
        .map(|r| task_v10(r, include_artifacts, a2ui))
        .collect();
    let next = if offset + page_size < total {
        encode_cursor(offset + page_size)
    } else {
        String::new()
    };
    Ok(json!({ "tasks": page, "nextPageToken": next, "pageSize": page_size, "totalSize": total }))
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// How a stream frames one event.
#[derive(Clone)]
pub(crate) enum Framing {
    /// 1.0 JSON-RPC: `{jsonrpc, id, result: StreamResponse}`.
    JsonRpc10(Value),
    /// 0.3 JSON-RPC: `{jsonrpc, id, result: <kind-tagged event>}`.
    JsonRpc03(Value),
    /// 1.0 HTTP+JSON: a bare StreamResponse.
    Rest,
}

impl Framing {
    fn wrap(&self, v: Value) -> String {
        match self {
            Framing::JsonRpc10(id) | Framing::JsonRpc03(id) => {
                json!({ "jsonrpc": "2.0", "id": id, "result": v }).to_string()
            }
            Framing::Rest => v.to_string(),
        }
    }
    fn v03(&self) -> bool {
        matches!(self, Framing::JsonRpc03(_))
    }
}

/// The frames for one live event of task `rec`, in the stream's version.
/// `None` ends nothing; terminal events are the caller's to detect.
fn frames_for(framing: &Framing, rec: &TaskRecord, ev: &TaskEvent, a2ui: bool) -> Vec<Value> {
    let (id, ctx) = (rec.id.as_str(), rec.context_id.as_str());
    let now = rfc3339(crate::tasks::now_ms());
    let v03 = framing.v03();
    let status_update = |state: TaskState, text: Option<&str>, meta: Option<Value>| {
        if v03 {
            let mut status = json!({ "state": state_v03(state), "timestamp": now });
            if let Some(t) = text {
                status["message"] = agent_message_v03(id, ctx, "status", t);
            }
            let mut u = json!({ "kind": "status-update", "taskId": id, "contextId": ctx,
                "status": status, "final": state.is_terminal() });
            if let Some(m) = meta {
                u["metadata"] = m;
            }
            u
        } else {
            let mut status = json!({ "state": state_v10(state), "timestamp": now });
            if let Some(t) = text {
                status["message"] = agent_message_v10(id, ctx, "status", t);
            }
            let mut u = json!({ "taskId": id, "contextId": ctx, "status": status });
            if let Some(m) = meta {
                u["metadata"] = m;
            }
            json!({ "statusUpdate": u })
        }
    };
    match ev {
        TaskEvent::Token(_) => Vec::new(),
        TaskEvent::Async {
            check_after_seconds,
            ..
        } => {
            let meta = check_after_seconds.map(|s| json!({ "checkAfterSeconds": s }));
            vec![status_update(TaskState::Working, None, meta)]
        }
        TaskEvent::Progress(p) => {
            let mut m = serde_json::Map::new();
            if let Some(step) = &p.step {
                m.insert("step".into(), json!(step));
            }
            if let Some(pct) = p.percent {
                m.insert("percent".into(), json!(pct));
            }
            let meta = (!m.is_empty()).then_some(Value::Object(m));
            vec![status_update(TaskState::Working, Some(&p.text), meta)]
        }
        TaskEvent::Done(_) => {
            let mut done = rec.clone();
            done.state = TaskState::Completed;
            let artifact = if v03 {
                let a = artifact_v03(&done).unwrap_or(Value::Null);
                json!({ "kind": "artifact-update", "taskId": id, "contextId": ctx,
                    "artifact": a, "append": false, "lastChunk": true })
            } else {
                let a = artifact_v10(&done, a2ui).unwrap_or(Value::Null);
                json!({ "artifactUpdate": { "taskId": id, "contextId": ctx,
                    "artifact": a, "append": false, "lastChunk": true } })
            };
            vec![artifact, status_update(TaskState::Completed, None, None)]
        }
        TaskEvent::Failed { class, message } => {
            let text = crate::tasks::user_text(class, message);
            vec![status_update(TaskState::Failed, Some(&text), None)]
        }
    }
}

fn sse(rx: tokio::sync::mpsc::Receiver<String>) -> Response {
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|data| (Ok::<_, Infallible>(Event::default().data(data)), rx))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

/// An SSE response: the task as it is now, then its live events until it
/// ends, the client goes away, or the stream limit passes.
pub(crate) fn task_stream(
    a2a: &A2aState,
    framing: Framing,
    first: TaskRecord,
    rx: Option<broadcast::Receiver<TaskEvent>>,
    a2ui: bool,
) -> Response {
    let (tx, out) = tokio::sync::mpsc::channel::<String>(32);
    let store = a2a.tasks.clone();
    let limit = a2a.task_settings.stream_max;
    tokio::spawn(async move {
        let initial = if framing.v03() {
            task_v03(&first)
        } else {
            json!({ "task": task_v10(&first, true, a2ui) })
        };
        if tx.send(framing.wrap(initial)).await.is_err() {
            return;
        }
        let Some(mut rx) = rx.filter(|_| !first.state.is_terminal()) else {
            return;
        };
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    // The runner ended between the snapshot and now; emit
                    // what the store has, if terminal.
                    if let Some(rec) = store.get(&first.id)
                        && rec.state.is_terminal()
                    {
                        let ev = match rec.state {
                            TaskState::Completed => TaskEvent::Done(Value::Null),
                            _ => TaskEvent::Failed {
                                class: rec.error_class.clone().unwrap_or_else(|| "tool".into()),
                                message: rec.error.clone().unwrap_or_default(),
                            },
                        };
                        for f in frames_for(&framing, &rec, &ev, a2ui) {
                            let _ = tx.send(framing.wrap(f)).await;
                        }
                    }
                    return;
                }
                Ok(Ok(ev)) => ev,
            };
            // Re-read so artifacts carry the stored result text.
            let rec = store.get(&first.id).unwrap_or_else(|| first.clone());
            for f in frames_for(&framing, &rec, &ev, a2ui) {
                if tx.send(framing.wrap(f)).await.is_err() {
                    return;
                }
            }
            if ev.is_terminal() {
                return;
            }
        }
    });
    sse(out)
}

/// A JSON-RPC error as an event stream of one event (0.3 clients read a
/// streaming method's answer as a stream only, errors included).
pub(crate) fn error_stream(body: Value) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(1);
    let _ = tx.try_send(body.to_string());
    sse(rx)
}

async fn op_stream(
    state: &SpecState,
    principal: triton_core::Principal,
    params: &Value,
    framing: Framing,
    a2ui: bool,
) -> Result<Response, A2aError> {
    let incoming = parse_send(params)?;
    if let Some(task_id) = &incoming.task_id {
        return Err(refuse_continuation(&state.a2a, &principal, task_id));
    }
    let context_id = incoming
        .context_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let overrides = crate::a2a_spec::turn_context_for(state, &context_id, a2ui);
    let (rec, rx) = start_task(state, &principal, &incoming.text, &context_id, overrides)?;
    Ok(task_stream(&state.a2a, framing, rec, Some(rx), a2ui))
}

/// Resubscribe (1.0 `SubscribeToTask`, 0.3 `tasks/resubscribe`): the task
/// now, then its events — resuming the upstream task when no runner in
/// this process drives it (after a restart).
pub(crate) fn op_subscribe(
    state: &SpecState,
    principal: &triton_core::Principal,
    params: &Value,
    framing: Framing,
    a2ui: bool,
) -> Result<Response, A2aError> {
    let id = opt_str(params, "id", "id")
        .ok_or_else(|| A2aError::InvalidParams("id is required".into()))?;
    let (rec, rx) = readable_live(&state.a2a, principal, &id)?;
    if rec.state.is_terminal() {
        return Err(A2aError::Unsupported(
            "The task is finished; read it with GetTask".into(),
        ));
    }
    Ok(task_stream(&state.a2a, framing, rec, rx, a2ui))
}

// ---------------------------------------------------------------------------
// 1.0 JSON-RPC
// ---------------------------------------------------------------------------

pub(crate) fn is_v10_method(method: &str) -> bool {
    matches!(
        method,
        "SendMessage"
            | "SendStreamingMessage"
            | "GetTask"
            | "ListTasks"
            | "CancelTask"
            | "SubscribeToTask"
            | "CreateTaskPushNotificationConfig"
            | "GetTaskPushNotificationConfig"
            | "ListTaskPushNotificationConfigs"
            | "DeleteTaskPushNotificationConfig"
            | "GetExtendedAgentCard"
    )
}

pub(crate) async fn jsonrpc_v10(
    state: SpecState,
    principal: triton_core::Principal,
    id: Value,
    method: &str,
    params: Value,
    a2ui: bool,
) -> Response {
    let result = match method {
        "SendMessage" => op_send(&state, principal, &params, a2ui).await,
        "SendStreamingMessage" => {
            return match op_stream(
                &state,
                principal,
                &params,
                Framing::JsonRpc10(id.clone()),
                a2ui,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => rpc_error_v10(&id, &e),
            };
        }
        "GetTask" => op_get(&state, &principal, &params, a2ui),
        "ListTasks" => op_list(&state, &principal, &params, a2ui),
        "CancelTask" => Err(op_cancel(&state, &principal, &params)),
        "SubscribeToTask" => {
            return match op_subscribe(
                &state,
                &principal,
                &params,
                Framing::JsonRpc10(id.clone()),
                a2ui,
            ) {
                Ok(r) => r,
                Err(e) => rpc_error_v10(&id, &e),
            };
        }
        "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => Err(A2aError::PushNotSupported),
        "GetExtendedAgentCard" => Err(A2aError::Unsupported(
            "This agent has no extended card; the public card is complete".into(),
        )),
        other => Err(A2aError::MethodNotFound(format!(
            "Method `{other}` not found; A2A 1.0 methods: SendMessage, SendStreamingMessage, \
             GetTask, ListTasks, CancelTask, SubscribeToTask"
        ))),
    };
    match result {
        Ok(v) => rpc_ok(&id, v),
        Err(e) => rpc_error_v10(&id, &e),
    }
}

// ---------------------------------------------------------------------------
// 1.0 HTTP+JSON binding
// ---------------------------------------------------------------------------

/// The HTTP+JSON routes, to be merged into the router nested at the A2A
/// endpoint (they answer at `<endpoint>/v1/...`).
pub fn rest_router(state: A2aState, config: Arc<SpecA2aConfig>) -> Router {
    Router::new()
        .route("/v1/message:send", post(rest_send))
        .route("/v1/message:stream", post(rest_stream))
        .route("/v1/tasks", get(rest_list))
        .route("/v1/tasks/{id}", get(rest_task_get).post(rest_task_post))
        .route(
            "/v1/tasks/{id}/pushNotificationConfigs",
            get(rest_push).post(rest_push),
        )
        .route(
            "/v1/tasks/{id}/pushNotificationConfigs/{config}",
            get(rest_push).delete(rest_push),
        )
        .route("/v1/extendedAgentCard", get(rest_extended_card))
        .with_state(SpecState { a2a: state, config })
        .layer(axum::middleware::from_fn(crate::identity::bearer_challenge))
}

/// Authentication, then the version: the HTTP+JSON binding is 1.0 only.
async fn rest_session(
    state: &SpecState,
    parts: &Parts,
    op: &str,
) -> Result<triton_core::Principal, Response> {
    let principal = match state.a2a.identity.verify(parts).await {
        Ok(p) => p,
        Err(e) => {
            state.a2a.dispatcher.record_rejection(
                op,
                "a2a",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            let a2a = match e {
                triton_core::TritonError::Forbidden(_) => A2aError::PermissionDenied(e.to_string()),
                _ => A2aError::Unauthenticated(e.to_string()),
            };
            return Err(state.a2a.identity.tag_rejection(&e, rest_error(&a2a)));
        }
    };
    match requested_version(&parts.headers, parts.uri.query()) {
        Ok(Version::V10) => Ok(principal),
        Ok(Version::V03) => Err(rest_error(&A2aError::VersionNotSupported(
            parts
                .headers
                .get("a2a-version")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        ))),
        Err(raw) => Err(rest_error(&A2aError::VersionNotSupported(raw))),
    }
}

fn rest_body(body: &Bytes) -> Result<Value, A2aError> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
        .ok_or(A2aError::Parse)
}

fn query_params(parts: &Parts) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes()) {
        if !k.eq_ignore_ascii_case("a2a-version") {
            m.insert(k.into_owned(), json!(v.into_owned()));
        }
    }
    Value::Object(m)
}

async fn rest_send(State(state): State<SpecState>, parts: Parts, body: Bytes) -> Response {
    let principal = match rest_session(&state, &parts, "SendMessage").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let a2ui = crate::a2a_spec::activates_a2ui_headers(&parts.headers);
    match rest_body(&body) {
        Ok(params) => match op_send(&state, principal, &params, a2ui).await {
            Ok(v) => rest_ok(v),
            Err(e) => rest_error(&e),
        },
        Err(e) => rest_error(&e),
    }
}

async fn rest_stream(State(state): State<SpecState>, parts: Parts, body: Bytes) -> Response {
    let principal = match rest_session(&state, &parts, "SendStreamingMessage").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let a2ui = crate::a2a_spec::activates_a2ui_headers(&parts.headers);
    match rest_body(&body) {
        Ok(params) => match op_stream(&state, principal, &params, Framing::Rest, a2ui).await {
            Ok(r) => r,
            Err(e) => rest_error(&e),
        },
        Err(e) => rest_error(&e),
    }
}

async fn rest_list(State(state): State<SpecState>, parts: Parts) -> Response {
    let principal = match rest_session(&state, &parts, "ListTasks").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let a2ui = crate::a2a_spec::activates_a2ui_headers(&parts.headers);
    match op_list(&state, &principal, &query_params(&parts), a2ui) {
        Ok(v) => rest_ok(v),
        Err(e) => rest_error(&e),
    }
}

/// `GET /tasks/{id}` and `GET /tasks/{id}:subscribe`.
async fn rest_task_get(
    State(state): State<SpecState>,
    Path(id): Path<String>,
    parts: Parts,
) -> Response {
    let principal = match rest_session(&state, &parts, "GetTask").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let a2ui = crate::a2a_spec::activates_a2ui_headers(&parts.headers);
    match id.strip_suffix(":subscribe") {
        Some(id) => match op_subscribe(
            &state,
            &principal,
            &json!({ "id": id }),
            Framing::Rest,
            a2ui,
        ) {
            Ok(r) => r,
            Err(e) => rest_error(&e),
        },
        None => match op_get(&state, &principal, &json!({ "id": id }), a2ui) {
            Ok(v) => rest_ok(v),
            Err(e) => rest_error(&e),
        },
    }
}

/// `POST /tasks/{id}:cancel` and `POST /tasks/{id}:subscribe`.
async fn rest_task_post(
    State(state): State<SpecState>,
    Path(id): Path<String>,
    parts: Parts,
) -> Response {
    let principal = match rest_session(&state, &parts, "CancelTask").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let a2ui = crate::a2a_spec::activates_a2ui_headers(&parts.headers);
    if let Some(id) = id.strip_suffix(":cancel") {
        return rest_error(&op_cancel(&state, &principal, &json!({ "id": id })));
    }
    if let Some(id) = id.strip_suffix(":subscribe") {
        return match op_subscribe(
            &state,
            &principal,
            &json!({ "id": id }),
            Framing::Rest,
            a2ui,
        ) {
            Ok(r) => r,
            Err(e) => rest_error(&e),
        };
    }
    rest_error(&A2aError::MethodNotFound("no such task operation".into()))
}

async fn rest_push(State(state): State<SpecState>, parts: Parts) -> Response {
    match rest_session(&state, &parts, "PushNotificationConfig").await {
        Ok(_) => rest_error(&A2aError::PushNotSupported),
        Err(r) => r,
    }
}

async fn rest_extended_card(State(state): State<SpecState>, parts: Parts) -> Response {
    match rest_session(&state, &parts, "GetExtendedAgentCard").await {
        Ok(_) => rest_error(&A2aError::Unsupported(
            "This agent has no extended card; the public card is complete".into(),
        )),
        Err(r) => r,
    }
}

// ---------------------------------------------------------------------------
// Cards
// ---------------------------------------------------------------------------

/// The skills a card advertises: configured, else the in-process registry
/// when it holds the default tool (the embedded host), else one skill for
/// the default tool (a binary in front of an upstream).
fn skills(state: &CardState) -> Vec<Value> {
    if let Some(s) = &state.config.skills {
        return s.clone();
    }
    let descriptors = state.dispatcher.descriptors();
    if descriptors
        .iter()
        .any(|d| d.name == state.config.default_tool)
    {
        return descriptors
            .into_iter()
            .map(|d| {
                json!({ "id": d.name, "name": d.name,
                    "description": format!("Triton tool `{}`", d.name), "tags": ["triton"] })
            })
            .collect();
    }
    let tool = &state.config.default_tool;
    vec![
        json!({ "id": tool, "name": state.config.name, "description": state.config.description,
        "tags": ["triton"] }),
    ]
}

/// OpenID Connect discovery URL for an accepted issuer. The Entra
/// multi-tenant sentinel names no tenant; `organizations` is the
/// authority's multi-tenant discovery document.
fn discovery_url(issuer: &str) -> String {
    let iss = issuer
        .trim_end_matches('/')
        .replace("{tenantid}", "organizations");
    format!("{iss}/.well-known/openid-configuration")
}

fn bearer_description(state: &CardState) -> String {
    state
        .oidc_providers
        .iter()
        .map(|(iss, aud)| format!("issuer {iss} / audience {aud}"))
        .collect::<Vec<_>>()
        .join("; or ")
}

fn a2ui_extension() -> Value {
    json!({
        "uri": triton_core::a2ui::ge::EXTENSION_URI,
        "description": "Ability to render A2UI v0.9",
        "required": false,
        "params": {
            "supportedCatalogIds": [triton_core::a2ui::ge::BASIC_CATALOG],
            "acceptsInlineCatalogs": false,
        },
    })
}

/// The 1.0 card.
pub(crate) fn card_v10(state: &CardState) -> Value {
    let cfg = &state.config;
    let url = cfg.endpoint_url();
    let mut card = json!({
        "name": cfg.name,
        "description": cfg.description,
        "version": cfg.version,
        "supportedInterfaces": [
            { "url": url, "protocolBinding": "JSONRPC", "protocolVersion": V10 },
            { "url": format!("{url}/v1"), "protocolBinding": "HTTP+JSON", "protocolVersion": V10 },
            { "url": url, "protocolBinding": "JSONRPC", "protocolVersion": "0.3" },
        ],
        "capabilities": {
            "streaming": true,
            "pushNotifications": false,
            "extendedAgentCard": false,
            "extensions": [a2ui_extension()],
        },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": skills(state),
    });
    if !state.oidc_providers.is_empty() {
        let mut schemes = serde_json::Map::new();
        schemes.insert(
            "bearer".into(),
            json!({ "httpAuthSecurityScheme": { "scheme": "bearer", "bearerFormat": "JWT",
                "description": bearer_description(state) } }),
        );
        for (i, (iss, aud)) in state.oidc_providers.iter().enumerate() {
            schemes.insert(
                format!("oidc_{i}"),
                json!({ "openIdConnectSecurityScheme": {
                    "description": format!("OpenID Connect discovery for {iss} (audience {aud})"),
                    "openIdConnectUrl": discovery_url(iss),
                } }),
            );
        }
        card["securitySchemes"] = Value::Object(schemes);
        card["securityRequirements"] = json!([{ "schemes": { "bearer": { "list": [] } } }]);
    }
    card
}

/// The 0.3.0 card (the shape Gemini Enterprise and Copilot Studio
/// registered against): `url` + `preferredTransport`, OpenAPI-style
/// `securitySchemes`, and `security` naming only the wire scheme.
pub(crate) fn card_v03(state: &CardState) -> Value {
    let cfg = &state.config;
    let mut card = json!({
        "protocolVersion": "0.3.0",
        "name": cfg.name,
        "description": cfg.description,
        "url": cfg.endpoint_url(),
        "preferredTransport": "JSONRPC",
        "version": cfg.version,
        "capabilities": {
            "streaming": true,
            "pushNotifications": false,
            "extensions": [a2ui_extension()],
        },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": skills(state),
    });
    if !state.oidc_providers.is_empty() {
        // `security` lists ONLY the wire scheme (`bearer`): listing the
        // `openIdConnect` discovery schemes as requirements makes Power
        // Platform's dispatcher abort with SystemError (2026-09-19).
        let mut schemes = serde_json::Map::new();
        schemes.insert(
            "bearer".into(),
            json!({ "type": "http", "scheme": "bearer", "bearerFormat": "JWT",
                "description": bearer_description(state) }),
        );
        for (i, (iss, aud)) in state.oidc_providers.iter().enumerate() {
            schemes.insert(
                format!("oidc_{i}"),
                json!({ "type": "openIdConnect",
                    "description": format!("OpenID Connect discovery for {iss} (audience {aud})"),
                    "openIdConnectUrl": discovery_url(iss) }),
            );
        }
        card["securitySchemes"] = Value::Object(schemes);
        card["security"] = json!([{ "bearer": [] }]);
    }
    card
}

/// Serve the card in the version the query selects, `default` otherwise;
/// an unknown version is a 400.
pub(crate) fn card_response(state: &CardState, query: Option<&str>, default: Version) -> Response {
    let wanted = query.and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "version")
            .map(|(_, v)| v.into_owned())
    });
    let version = match wanted.as_deref().map(str::trim) {
        None | Some("") => default,
        Some(raw) => match parse_version(raw) {
            Some(v) => v,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "version_not_supported",
                        "supported_versions": ["0.3", "1.0"] })),
                )
                    .into_response();
            }
        },
    };
    let card = match version {
        Version::V10 => card_v10(state),
        Version::V03 => card_v03(state),
    };
    let mut resp = (StatusCode::OK, axum::Json(card)).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_select_0_3_or_1_0() {
        assert_eq!(parse_version(""), Some(Version::V03));
        assert_eq!(parse_version("0.3"), Some(Version::V03));
        assert_eq!(parse_version("0.3.0"), Some(Version::V03));
        assert_eq!(parse_version("1.0"), Some(Version::V10));
        assert_eq!(parse_version(" 1.0.2 "), Some(Version::V10));
        for bad in ["2.0", "1", "1.0.", "1.0.x", "0.30", "v1.0"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_793_448_000_123), "2026-10-31T12:00:00.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn rfc3339_parses_offsets_and_fractions_and_round_trips() {
        for ms in [0, 951_782_400_000, 1_793_448_000_123] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms));
        }
        assert_eq!(
            parse_rfc3339("2026-10-31T14:00:00.5+02:00"),
            parse_rfc3339("2026-10-31T12:00:00.500Z")
        );
        assert!(
            parse_rfc3339("2026-10-31T12:00:00Z").unwrap()
                < parse_rfc3339("2026-10-31T12:00:00.5Z").unwrap()
        );
        for bad in [
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-10-31",
            "2026-10-31T25:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn cursors_round_trip_and_reject_foreign_tokens() {
        assert_eq!(decode_cursor(&encode_cursor(40)), Some(40));
        assert_eq!(decode_cursor("bm9wZQ"), None);
    }

    #[test]
    fn error_forms_differ_by_version() {
        let e = A2aError::VersionNotSupported("2.0".into());
        assert_eq!(
            e.body_v03()["data"],
            json!({ "requestedVersion": "2.0", "supportedVersions": ["0.3", "1.0"] })
        );
        assert_eq!(e.body_v10()["data"][0]["reason"], "VERSION_NOT_SUPPORTED");
        assert!(
            A2aError::MethodNotFound("x".into())
                .body_v10()
                .get("data")
                .is_none()
        );
    }
}
