//! Test fixtures for the upstream dispatch path: a tiny axum server
//! that speaks the actual upstream-agent wire shape. No mocks per
//! CLAUDE.md §1.
//!
//! The Consul + Vault fakes were removed with the decommission of the
//! HashiCorp stack; `StaticUpstream` dispatches straight to a fixed
//! `host:port`, so only [`FakeAgent`] is needed.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Fake upstream agent. Several profiles:
/// * `echoing` — accepts anything, echoes the body back.
/// * `always_failing` — returns 500 on every request.
/// * `failing_then_recovering(n)` — fails the first `n` calls, then
///   recovers.
/// * `returning(json)` — responds with a fixed JSON body on every
///   request (used by the upstream identity-resolver tests, where the
///   resolver agent returns a `{sub, scopes, tenant}` principal).
pub struct FakeAgent {
    addr: SocketAddr,
    state: Arc<FakeAgentState>,
}

struct FakeAgentState {
    mode: Mutex<AgentMode>,
    bearers_seen: Mutex<Vec<String>>,
    /// `X-Triton-Tool` header per request (`None` when absent) — lets
    /// tests pin the dispatch-header contract.
    tools_seen: Mutex<Vec<Option<String>>>,
    bodies_seen: Mutex<Vec<Value>>,
    hits: Mutex<u32>,
    failures_remaining: Mutex<u32>,
    /// Fixed response body for `AgentMode::Returning`.
    fixed_response: Option<Value>,
}

#[derive(Clone, Copy)]
enum AgentMode {
    Echo,
    AlwaysFail,
    FailingThenRecover,
    Returning,
    /// Emit a `text/event-stream` response — `tool`/`token` progress
    /// frames followed by a terminal `done`, each flushed separately
    /// with a small delay so a consumer observes them incrementally
    /// (issue #132). The `done` payload echoes the request body under
    /// `echoed`, like [`AgentMode::Echo`].
    Streaming,
    /// Stream a couple of frames, then drop the connection **without** a
    /// terminal `done`/`error` — exercises the upstream-truncation path.
    StreamingTruncated,
    /// Like [`AgentMode::Streaming`], but the terminal `done` carries an
    /// A2UI `{ surface: { components: [...] } }` payload so the streaming
    /// A2UI-wrap path can be exercised end-to-end.
    StreamingSurface,
}

impl FakeAgent {
    pub async fn start_echoing() -> Self {
        Self::start(AgentMode::Echo, 0, None).await
    }

    pub async fn start_always_failing() -> Self {
        Self::start(AgentMode::AlwaysFail, 0, None).await
    }

    pub async fn start_failing_then_recovering(fail_first: u32) -> Self {
        Self::start(AgentMode::FailingThenRecover, fail_first, None).await
    }

    /// Respond with `body` (status 200) on every request, ignoring the
    /// request body. The upstream dispatcher returns this verbatim as
    /// the tool result.
    pub async fn start_returning(body: Value) -> Self {
        Self::start(AgentMode::Returning, 0, Some(body)).await
    }

    /// Emit an incremental SSE stream (`tool`/`token`/`done`). See
    /// [`AgentMode::Streaming`].
    pub async fn start_streaming() -> Self {
        Self::start(AgentMode::Streaming, 0, None).await
    }

    /// Emit a couple of SSE frames then drop without a terminal event.
    /// See [`AgentMode::StreamingTruncated`].
    pub async fn start_streaming_truncated() -> Self {
        Self::start(AgentMode::StreamingTruncated, 0, None).await
    }

    /// Emit an SSE stream whose terminal `done` carries an A2UI surface.
    /// See [`AgentMode::StreamingSurface`].
    pub async fn start_streaming_surface() -> Self {
        Self::start(AgentMode::StreamingSurface, 0, None).await
    }

    async fn start(mode: AgentMode, fail_first: u32, fixed_response: Option<Value>) -> Self {
        let state = Arc::new(FakeAgentState {
            mode: Mutex::new(mode),
            bearers_seen: Mutex::new(Vec::new()),
            tools_seen: Mutex::new(Vec::new()),
            bodies_seen: Mutex::new(Vec::new()),
            hits: Mutex::new(0),
            failures_remaining: Mutex::new(fail_first),
            fixed_response,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();

        let router = Router::new()
            .route("/", any(handler))
            .route("/{*rest}", any(handler))
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn host_port(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    pub fn bearers_seen(&self) -> Vec<String> {
        self.state.bearers_seen.lock().unwrap().clone()
    }

    /// The `X-Triton-Tool` header of each request, `None` where the
    /// dispatcher omitted it.
    pub fn tools_seen(&self) -> Vec<Option<String>> {
        self.state.tools_seen.lock().unwrap().clone()
    }

    /// JSON bodies of every request this agent received, in order.
    /// Unparseable bodies are recorded as `Value::Null`.
    pub fn bodies_seen(&self) -> Vec<Value> {
        self.state.bodies_seen.lock().unwrap().clone()
    }

    pub fn hits(&self) -> u32 {
        *self.state.hits.lock().unwrap()
    }

    pub fn reset_hits(&self) {
        *self.state.hits.lock().unwrap() = 0;
    }
}

async fn handler(
    State(state): State<Arc<FakeAgentState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    *state.hits.lock().unwrap() += 1;
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string())
        .unwrap_or_default();
    state.bearers_seen.lock().unwrap().push(bearer);
    let tool = headers
        .get("X-Triton-Tool")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    state.tools_seen.lock().unwrap().push(tool);
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.bodies_seen.lock().unwrap().push(value.clone());

    let mode = *state.mode.lock().unwrap();
    let should_fail = match mode {
        AgentMode::Echo
        | AgentMode::Returning
        | AgentMode::Streaming
        | AgentMode::StreamingTruncated
        | AgentMode::StreamingSurface => false,
        AgentMode::AlwaysFail => true,
        AgentMode::FailingThenRecover => {
            let mut left = state.failures_remaining.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                true
            } else {
                false
            }
        }
    };
    if should_fail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "fake agent configured to fail" })),
        )
            .into_response();
    }

    match mode {
        AgentMode::Returning => {
            let body = state.fixed_response.clone().unwrap_or(Value::Null);
            Json(body).into_response()
        }
        AgentMode::Streaming => sse_response(streaming_frames(&value, true)),
        AgentMode::StreamingTruncated => sse_response(streaming_frames(&value, false)),
        AgentMode::StreamingSurface => sse_response(streaming_surface_frames()),
        _ => Json(json!({ "echoed": value })).into_response(),
    }
}

/// Progress frames followed by a terminal `done` carrying an A2UI
/// surface — the shape `extract_surface` parses.
fn streaming_surface_frames() -> Vec<String> {
    let done = json!({
        "surface": { "components": [ { "kind": "text", "value": "Hello world" } ] }
    });
    vec![
        "event: tool\ndata: {\"step\":\"search\",\"hits\":30}\n\n".to_string(),
        "event: token\ndata: {\"delta\":\"Hello \"}\n\n".to_string(),
        format!("event: done\ndata: {done}\n\n"),
    ]
}

/// The SSE frames a streaming agent emits. When `terminal` is true the
/// sequence ends with a `done` carrying `{ "echoed": <body> }` (mirroring
/// the echo agent); otherwise it stops after the progress frames to
/// simulate an upstream that closed early.
fn streaming_frames(body: &Value, terminal: bool) -> Vec<String> {
    let mut frames = vec![
        "event: tool\ndata: {\"step\":\"search\",\"hits\":30}\n\n".to_string(),
        "event: tool\ndata: {\"step\":\"acl\",\"kept\":12}\n\n".to_string(),
        "event: token\ndata: {\"delta\":\"Hello \"}\n\n".to_string(),
        "event: token\ndata: {\"delta\":\"world\"}\n\n".to_string(),
    ];
    if terminal {
        let done = json!({ "echoed": body });
        frames.push(format!("event: done\ndata: {done}\n\n"));
    }
    frames
}

/// Build a `text/event-stream` response that flushes each frame
/// separately with a ~15ms gap, so a consumer measurably observes the
/// frames arriving over time (TTFB < total).
fn sse_response(frames: Vec<String>) -> axum::response::Response {
    let body = Body::from_stream(futures::stream::unfold(
        (frames.into_iter(), false),
        |(mut it, started)| async move {
            if started {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            it.next()
                .map(|frame| (Ok::<_, Infallible>(frame.into_bytes()), (it, true)))
        },
    ));
    axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap()
}
