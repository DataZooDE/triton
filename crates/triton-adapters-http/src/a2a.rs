//! A2A adapter — `POST /message:send` per FR-A-7.
//!
//! A2A's wire shape is `Message{parts: [Part{data}], metadata}`.
//! The adapter is a thin unwrap/wrap shell per ADR-6: it parses the
//! Message, extracts `(tool, args)` from `parts[0].data`, lets the
//! dispatcher run, and wraps the result back into a response Message.
//!
//! G-8 forbids on-disk state — but the A2A protocol expects a task
//! store. [`InMemoryTaskStore`] is the FR-A-7 stub: it tracks
//! `trace_id → status` in process memory so future routes
//! (`task:get`, `task:cancel`) can plug in without touching the
//! adapter signature. By construction, restart wipes the store.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, extract_surface};
use triton_core::stream::{Finalized, Termination};
use triton_core::{A2uiVersion, Dispatcher, TritonError, envelope};

use crate::identity::IdentityProvider;
use crate::rest::{sse_response, wants_sse};

/// The task store and state now live in [`crate::tasks`]; these
/// names keep the adapter's public surface.
pub use crate::tasks::{TaskState, TaskStore};

/// The historical name of the store: process memory by default.
pub type InMemoryTaskStore = TaskStore;

#[derive(Clone)]
pub struct A2aState {
    pub dispatcher: Arc<Dispatcher>,
    pub tasks: TaskStore,
    /// Live tasks of this process.
    pub hub: crate::tasks::TaskHub,
    /// Block, stream and task limits.
    pub task_settings: Arc<crate::tasks::TaskSettings>,
    pub identity: Arc<IdentityProvider>,
    /// #306 crew F1: `tasks/get` answers on a trace id, so it needs the
    /// same tenant-scoping inputs `/v1/trace` uses. Parsed once at boot,
    /// like everywhere else.
    pub audit_operators: Arc<std::collections::HashSet<(String, String)>>,
}

impl A2aState {
    /// The RESOLVED deployment environment — never
    /// `std::env::var("TRITON_ENV")`, which clap reads without setting.
    pub fn env(&self) -> &str {
        self.dispatcher.env()
    }

    /// The runner that drives this state's long-running tasks.
    pub fn runner(&self) -> crate::tasks::Runner {
        crate::tasks::Runner {
            dispatcher: self.dispatcher.clone(),
            store: self.tasks.clone(),
            hub: self.hub.clone(),
            settings: self.task_settings.clone(),
            render: std::sync::Arc::new(crate::a2a_spec::reply_text),
        }
    }
}

pub fn router(state: A2aState) -> Router {
    Router::new()
        .route("/message:send", post(message_send))
        .with_state(state)
        .layer(axum::middleware::from_fn(crate::identity::bearer_challenge))
}

#[derive(Debug, Deserialize)]
struct InboundMessage {
    #[serde(default)]
    parts: Vec<InboundPart>,
    #[serde(default)]
    metadata: Option<InboundMetadata>,
}

#[derive(Debug, Deserialize)]
struct InboundPart {
    data: InboundData,
}

#[derive(Debug, Deserialize)]
struct InboundData {
    tool: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Deserialize)]
struct InboundMetadata {
    #[serde(default)]
    a2ui_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutboundMessage {
    parts: Vec<OutboundPart>,
    metadata: OutboundMetadata,
}

#[derive(Debug, Serialize)]
struct OutboundPart {
    data: Value,
}

#[derive(Debug, Serialize)]
struct OutboundMetadata {
    trace_id: String,
    task_state: TaskState,
    /// The upstream's per-turn tool trace (`_meta.tool_trace`), mirrored onto
    /// A2A's `metadata` like `trace_id`. Absent when the upstream sent none, so
    /// existing consumers see a byte-identical message.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_trace: Option<Value>,
}

async fn message_send(State(state): State<A2aState>, parts: Parts, body: Bytes) -> Response {
    // Auth first — even a malformed body should produce a rejection
    // audit, but auth failures take precedence so an attacker
    // probing for body-shape errors doesn't get a 4xx ahead of the
    // credential check.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                "message:send",
                "a2a",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return state.identity.tag_rejection(&e, a2a_error(&e));
        }
    };

    // Parse the body ourselves rather than via `Json<InboundMessage>`
    // so a malformed body still flows through `record_rejection`
    // (Codex PR 4/PR 6 finding: axum's Json extractor 422s before
    // our handler runs, bypassing FR-AU-1).
    let parsed: InboundMessage = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = TritonError::Validation(format!("invalid A2A Message: {e}"));
            state.dispatcher.record_rejection(
                "message:send",
                "a2a",
                &principal.sub,
                &principal.tenant,
                &principal.trace_id,
                &err,
            );
            return a2a_error_with_trace(&err, &principal.trace_id);
        }
    };

    // FR-A-3: A2A MUST honour `metadata.a2ui_version`. Unknown
    // versions are rejected so callers don't silently downgrade.
    let requested = match parse_a2ui_version(&parsed.metadata) {
        Ok(v) => v,
        Err(err) => {
            state.dispatcher.record_rejection(
                "message:send",
                "a2a",
                &principal.sub,
                &principal.tenant,
                &principal.trace_id,
                &err,
            );
            return a2a_error_with_trace(&err, &principal.trace_id);
        }
    };

    // FR-A-7 wire shape: at least one Part, carrying tool+args.
    let Some(first) = parsed.parts.into_iter().next() else {
        let err = TritonError::Validation("A2A Message MUST contain at least one Part".into());
        state.dispatcher.record_rejection(
            "message:send",
            "a2a",
            &principal.sub,
            &principal.tenant,
            &principal.trace_id,
            &err,
        );
        return a2a_error_with_trace(&err, &principal.trace_id);
    };
    let tool_name = first.data.tool;
    let args = first.data.args;
    let trace_id = principal.trace_id.clone();

    // Content negotiation (issue #132): `Accept: text/event-stream`
    // streams the dispatch as SSE (`tool`/`token`/`done`/`error`
    // frames). The task store is recorded at stream termination via a
    // `Finalized` finalizer (Completed on a clean `done`, else Failed);
    // every other caller keeps the buffered Message response below.
    if wants_sse(&parts) {
        return match state
            .dispatcher
            .invoke_streaming(&tool_name, args, principal, "a2a", requested)
            .await
        {
            Ok(events) => {
                let tasks = state.tasks.clone();
                let task_trace = trace_id.clone();
                let finalized = Finalized::new(events, move |term: Termination, _timing| {
                    let task_state = if term.is_success() {
                        TaskState::Completed
                    } else {
                        TaskState::Failed
                    };
                    tasks.record(&task_trace, task_state);
                });
                sse_response(finalized.boxed())
            }
            // Pre-first-byte failure: no SSE headers flushed yet, so we
            // can answer with the ordinary A2A error Message (audited
            // once inside the dispatcher).
            Err(e) => {
                state.tasks.record(&trace_id, TaskState::Failed);
                a2a_error_with_trace(&e, &trace_id)
            }
        };
    }

    match state
        .dispatcher
        .invoke(&tool_name, args, principal, "a2a")
        .await
    {
        Ok(mut d) => {
            // Capture the upstream's tool trace before the A2UI wrap rewrites
            // `d.result` and drops `_meta` (mirrors the MCP/REST paths). Only a
            // structured array is reflected.
            let tool_trace = d
                .result
                .get("_meta")
                .and_then(|m| m.get("tool_trace"))
                .filter(|t| t.is_array())
                .cloned();
            match wrap_a2ui_if_requested(&mut d, requested) {
                Ok(()) => {
                    state.tasks.record(&trace_id, TaskState::Completed);
                    let msg = OutboundMessage {
                        parts: vec![OutboundPart { data: envelope(&d) }],
                        metadata: OutboundMetadata {
                            trace_id: d.trace_id.clone(),
                            task_state: TaskState::Completed,
                            tool_trace,
                        },
                    };
                    (StatusCode::OK, Json(msg)).into_response()
                }
                Err(e) => {
                    state.tasks.record(&trace_id, TaskState::Failed);
                    a2a_error_with_trace(&e, &trace_id)
                }
            }
        }
        Err(e) => {
            state.tasks.record(&trace_id, TaskState::Failed);
            a2a_error_with_trace(&e, &trace_id)
        }
    }
}

fn wrap_a2ui_if_requested(
    d: &mut triton_core::Dispatch,
    requested: Option<A2uiVersion>,
) -> Result<(), TritonError> {
    let Some(version) = requested else {
        return Ok(());
    };
    if !d.returns_a2ui {
        return Ok(());
    }
    let surface = extract_surface(&d.result).map_err(|e| {
        tracing::warn!(tool_advertised_a2ui = true, error = %e, "tool returned non-A2UI shape");
        TritonError::Tool(format!("tool advertised A2UI but {e}"))
    })?;
    d.result = build_envelope(&surface, version.into());
    Ok(())
}

fn parse_a2ui_version(
    metadata: &Option<InboundMetadata>,
) -> Result<Option<A2uiVersion>, TritonError> {
    let Some(md) = metadata else {
        return Ok(None);
    };
    let Some(v) = md.a2ui_version.as_deref() else {
        return Ok(None);
    };
    match v {
        "v0.8" => Ok(Some(A2uiVersion::V08)),
        "v0.9" => Ok(Some(A2uiVersion::V09)),
        other => Err(TritonError::Validation(format!(
            "unknown A2UI version: {other}"
        ))),
    }
}

fn a2a_error(e: &TritonError) -> Response {
    a2a_error_with_trace_opt(e, None)
}

fn a2a_error_with_trace(e: &TritonError, trace_id: &str) -> Response {
    a2a_error_with_trace_opt(e, Some(trace_id))
}

/// FR-D-4 mapping for A2A: error variant rides on
/// `Message.metadata.error` per the experiments' reference. HTTP
/// status mirrors REST so operators reading raw traffic see a
/// consistent signal across protocols (architecture.md §8.3).
fn a2a_error_with_trace_opt(e: &TritonError, trace_id: Option<&str>) -> Response {
    // Shared with REST + the dispatcher audit (architecture §8.3) so
    // circuit_open → 503 and upstream timeout → 504 here too, not 502.
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut metadata = json!({ "error": e.class(), "message": e.to_string() });
    if let Some(tid) = trace_id {
        metadata["trace_id"] = json!(tid);
    }
    // Top-level fields mirror the REST adapter's error JSON so a
    // tester can assert `body["error"]` without protocol-branching.
    let body = json!({
        "error": e.class(),
        "message": e.to_string(),
        "metadata": metadata,
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::{InMemoryTaskStore, TaskState};

    // Bounding and eviction are pinned in `tasks.rs`; this pins the legacy
    // entry points the Triton-shaped route uses.
    #[test]
    fn task_store_update_keeps_one_entry() {
        let store = InMemoryTaskStore::new();
        store.record("t", TaskState::Completed);
        store.record("t", TaskState::Failed);
        assert_eq!(store.state("t"), Some(TaskState::Failed));
        assert!(store.get("t").is_some());
    }
}
