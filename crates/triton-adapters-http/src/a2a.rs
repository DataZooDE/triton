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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, extract_surface};
use triton_core::{A2uiVersion, Dispatcher, TritonError, envelope};

use crate::identity::IdentityProvider;

/// Upper bound on retained tasks. Every inbound A2A call records one
/// entry; without a cap the map grows for the life of the process
/// (Codex robustness review). Bounded FIFO: the oldest entries
/// scroll off. Restart-clean by construction (G-8).
const TASK_STORE_CAPACITY: usize = 1024;

/// FR-A-7 task store. In-process memory; restart-clean by
/// construction (G-8). The echo tool finishes synchronously, so
/// every recorded task is immediately `Completed` or `Failed`.
/// Bounded at [`TASK_STORE_CAPACITY`] with FIFO eviction.
#[derive(Clone, Default)]
pub struct InMemoryTaskStore {
    inner: Arc<Mutex<TaskStoreInner>>,
}

#[derive(Default)]
struct TaskStoreInner {
    states: HashMap<String, TaskState>,
    /// Insertion order of the keys in `states`, for FIFO eviction.
    order: VecDeque<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Completed,
    Failed,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, trace_id: &str) -> Option<TaskState> {
        self.inner.lock().unwrap().states.get(trace_id).copied()
    }

    fn record(&self, trace_id: &str, state: TaskState) {
        let mut g = self.inner.lock().unwrap();
        // Only a brand-new key extends the store; an update keeps the
        // existing position so the FIFO order tracks first-seen.
        if g.states.insert(trace_id.to_string(), state).is_none() {
            g.order.push_back(trace_id.to_string());
            while g.order.len() > TASK_STORE_CAPACITY {
                if let Some(old) = g.order.pop_front() {
                    g.states.remove(&old);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct A2aState {
    pub dispatcher: Arc<Dispatcher>,
    pub tasks: InMemoryTaskStore,
    pub identity: Arc<IdentityProvider>,
}

pub fn router(state: A2aState) -> Router {
    Router::new()
        .route("/message:send", post(message_send))
        .with_state(state)
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
            return a2a_error(&e);
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

    match state
        .dispatcher
        .invoke(&tool_name, args, principal, "a2a")
        .await
    {
        Ok(mut d) => match wrap_a2ui_if_requested(&mut d, requested) {
            Ok(()) => {
                state.tasks.record(&trace_id, TaskState::Completed);
                let msg = OutboundMessage {
                    parts: vec![OutboundPart { data: envelope(&d) }],
                    metadata: OutboundMetadata {
                        trace_id: d.trace_id.clone(),
                        task_state: TaskState::Completed,
                    },
                };
                (StatusCode::OK, Json(msg)).into_response()
            }
            Err(e) => {
                state.tasks.record(&trace_id, TaskState::Failed);
                a2a_error_with_trace(&e, &trace_id)
            }
        },
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
    use super::{InMemoryTaskStore, TASK_STORE_CAPACITY, TaskState};

    #[test]
    fn task_store_is_bounded_and_evicts_oldest() {
        let store = InMemoryTaskStore::new();
        // Record well past capacity.
        for i in 0..(TASK_STORE_CAPACITY + 50) {
            store.record(&format!("trace-{i}"), TaskState::Completed);
        }
        // The map never exceeds the cap.
        assert_eq!(
            store.inner.lock().unwrap().states.len(),
            TASK_STORE_CAPACITY
        );
        // Oldest scrolled off; newest retained.
        assert!(store.get("trace-0").is_none());
        assert!(
            store
                .get(&format!("trace-{}", TASK_STORE_CAPACITY + 49))
                .is_some()
        );
    }

    #[test]
    fn task_store_update_keeps_one_entry() {
        let store = InMemoryTaskStore::new();
        store.record("t", TaskState::Completed);
        store.record("t", TaskState::Failed);
        assert_eq!(store.inner.lock().unwrap().states.len(), 1);
        assert_eq!(store.inner.lock().unwrap().order.len(), 1);
    }
}
