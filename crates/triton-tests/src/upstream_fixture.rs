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
    /// `X-Triton-MCP` header per request (`None` when absent) — pins the
    /// MCP-Apps verb-routing contract (#143 B/C).
    mcp_verbs_seen: Mutex<Vec<Option<String>>>,
    bodies_seen: Mutex<Vec<Value>>,
    hits: Mutex<u32>,
    failures_remaining: Mutex<u32>,
    /// Fixed response body for `AgentMode::Returning`.
    fixed_response: Option<Value>,
    /// Sleep this long before answering — a slow "thinking" agent
    /// (#164: live-LLM turns outlast Google Chat's ~30s webhook
    /// deadline; tests use ~2s to prove the ack doesn't wait).
    delay: Option<Duration>,
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
    /// Stream a couple of progress frames, then **hang** — never send
    /// another byte and never close the connection. Exercises TS-03: the
    /// per-frame idle timeout must fail the stream closed.
    StreamingHang,
    /// Stream a couple of progress frames, then **drip** a keep-alive
    /// comment byte every ~20ms forever, never sending a terminal event.
    /// The steady trickle keeps resetting the idle timer, so only the
    /// total-duration cap can stop it (Codex security review).
    StreamingSlowDrip,
    /// MCP-Apps upstream (#143). Routes on the `X-Triton-MCP` header:
    /// * absent (a normal tool call) → `{ report_id, _meta.ui.resourceUri }`
    ///   so the `tools/call` pass-through (A) and `callServerTool` (C) paths
    ///   have a UI-bearing result.
    /// * `resources/read` → `{ contents: [{ uri, mimeType, text }] }` echoing
    ///   the requested `uri` from the body (B).
    /// * `updateModelContext` → `{ "relayed": true }`; the pushed record is
    ///   captured verbatim in `bodies_seen` so the relay can be asserted
    ///   byte-for-byte (C).
    McpApps,
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

    /// Like [`start_returning`](Self::start_returning), but the response
    /// only arrives after `delay` — a slow agent whose dispatch outlasts
    /// a chat platform's synchronous webhook deadline (#164 T1a).
    pub async fn start_returning_after(delay: Duration, body: Value) -> Self {
        Self::start_with_delay(AgentMode::Returning, 0, Some(body), Some(delay)).await
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

    /// Emit a couple of progress frames then hang forever (no terminal,
    /// no close). See [`AgentMode::StreamingHang`].
    pub async fn start_streaming_hang() -> Self {
        Self::start(AgentMode::StreamingHang, 0, None).await
    }

    /// Emit a couple of progress frames then drip keep-alive bytes forever
    /// (no terminal). See [`AgentMode::StreamingSlowDrip`].
    pub async fn start_streaming_slow_drip() -> Self {
        Self::start(AgentMode::StreamingSlowDrip, 0, None).await
    }

    /// A full MCP-Apps upstream that serves tool calls, `resources/read`,
    /// and `updateModelContext` off the `X-Triton-MCP` header (#143).
    pub async fn start_mcp_apps() -> Self {
        Self::start(AgentMode::McpApps, 0, None).await
    }

    async fn start(mode: AgentMode, fail_first: u32, fixed_response: Option<Value>) -> Self {
        Self::start_with_delay(mode, fail_first, fixed_response, None).await
    }

    async fn start_with_delay(
        mode: AgentMode,
        fail_first: u32,
        fixed_response: Option<Value>,
        delay: Option<Duration>,
    ) -> Self {
        let state = Arc::new(FakeAgentState {
            mode: Mutex::new(mode),
            bearers_seen: Mutex::new(Vec::new()),
            tools_seen: Mutex::new(Vec::new()),
            mcp_verbs_seen: Mutex::new(Vec::new()),
            bodies_seen: Mutex::new(Vec::new()),
            hits: Mutex::new(0),
            failures_remaining: Mutex::new(fail_first),
            fixed_response,
            delay,
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

    /// The `X-Triton-MCP` verb header of each request, `None` where absent
    /// (e.g. a plain tool call). Pins the MCP-Apps routing contract.
    pub fn mcp_verbs_seen(&self) -> Vec<Option<String>> {
        self.state.mcp_verbs_seen.lock().unwrap().clone()
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
    let mcp_verb = headers
        .get("X-Triton-MCP")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Schema-discovery probes (`tools/list`) are infrastructure, not tool
    // calls — answer them WITHOUT recording, so tests asserting the upstream's
    // request history (hits / tools_seen / bodies_seen) aren't polluted. Only
    // an MCP-Apps agent advertises a schema; others 404 so the introspection
    // best-effort path falls back to an empty schema.
    if mcp_verb.as_deref() == Some("tools/list") {
        return match *state.mode.lock().unwrap() {
            AgentMode::McpApps => Json(json!({
                "tools": [{
                    "name": "render_report",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "report_id": { "type": "string" } },
                        "required": ["report_id"]
                    }
                }]
            }))
            .into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        };
    }

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
    state.mcp_verbs_seen.lock().unwrap().push(mcp_verb.clone());
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.bodies_seen.lock().unwrap().push(value.clone());

    // A slow "thinking" agent (#164): hold the response so callers that
    // must not block on dispatch can prove they didn't wait.
    if let Some(delay) = state.delay {
        tokio::time::sleep(delay).await;
    }

    let mode = *state.mode.lock().unwrap();
    let should_fail = match mode {
        AgentMode::Echo
        | AgentMode::Returning
        | AgentMode::Streaming
        | AgentMode::StreamingTruncated
        | AgentMode::StreamingSurface
        | AgentMode::StreamingHang
        | AgentMode::StreamingSlowDrip
        | AgentMode::McpApps => false,
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
        AgentMode::StreamingHang => sse_response_hanging(streaming_frames(&value, false)),
        AgentMode::StreamingSlowDrip => sse_response_slow_drip(streaming_frames(&value, false)),
        AgentMode::McpApps => mcp_apps_response(mcp_verb.as_deref(), &value),
        _ => Json(json!({ "echoed": value })).into_response(),
    }
}

/// MCP-Apps responder (#143), routing on the `X-Triton-MCP` verb:
/// no verb → a UI-bearing tool result; `resources/read` → a bundle whose
/// `uri` echoes the request; `updateModelContext` → an ack (the record is
/// already captured in `bodies_seen` for byte-for-byte relay assertions).
fn mcp_apps_response(verb: Option<&str>, body: &Value) -> axum::response::Response {
    match verb {
        Some("resources/read") => {
            let uri = body.get("uri").and_then(Value::as_str).unwrap_or("");
            Json(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/html",
                    "text": format!("<html><body>peacock bundle for {uri}</body></html>")
                }]
            }))
            .into_response()
        }
        Some("updateModelContext") => Json(json!({ "relayed": true })).into_response(),
        // A normal tool call (`render_report` / `callServerTool`).
        _ => Json(json!({
            "report_id": "r1",
            "_meta": { "ui": { "resourceUri": "ui://peacock/r1" } }
        }))
        .into_response(),
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

/// Like [`sse_response`], but after flushing the frames the body stream
/// **never yields again and never closes** — simulating a hung upstream.
/// The client's per-frame idle timeout (TS-03) is what must cut it.
fn sse_response_hanging(frames: Vec<String>) -> axum::response::Response {
    let framed = futures::stream::unfold(
        (frames.into_iter(), false),
        |(mut it, started)| async move {
            if started {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            it.next()
                .map(|frame| (Ok::<_, Infallible>(frame.into_bytes()), (it, true)))
        },
    );
    // Append a never-ready tail so the connection stays open with no
    // further bytes and no EOF.
    use futures::StreamExt as _;
    let body = Body::from_stream(framed.chain(futures::stream::pending()));
    axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap()
}

/// Like [`sse_response`], but after flushing the frames it drips a
/// keep-alive comment frame (`: ping`) every ~20ms **forever**, never
/// emitting a terminal event. The steady trickle resets the per-frame
/// idle timer indefinitely, so only the total-duration cap can end it.
fn sse_response_slow_drip(frames: Vec<String>) -> axum::response::Response {
    let framed = futures::stream::unfold(
        (frames.into_iter(), false),
        |(mut it, started)| async move {
            if started {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            it.next()
                .map(|frame| (Ok::<_, Infallible>(frame.into_bytes()), (it, true)))
        },
    );
    let drip = futures::stream::unfold((), |()| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A comment frame: bytes arrive (resetting the idle timer) but
        // decode to no event.
        Some((Ok::<_, Infallible>(b": ping\n\n".to_vec()), ()))
    });
    use futures::StreamExt as _;
    let body = Body::from_stream(framed.chain(drip));
    axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap()
}
