//! Spec-conformant A2A: the JSON-RPC 2.0 binding and the Agent Card.
//!
//! Triton's original A2A leg (`a2a.rs`) speaks a Triton-shaped message —
//! `POST /a2a/message:send` with `parts[0].data = {tool, args}` — which is
//! fine for callers that know Triton, and unusable by anything that only
//! knows the published A2A protocol. Agent platforms (Gemini Enterprise,
//! Microsoft Copilot Studio) discover an agent by fetching its Agent Card
//! and then call `POST <url>` with JSON-RPC `message/send`, carrying plain
//! text rather than a tool name.
//!
//! This module is that second face. It does not replace the first: both
//! mount on the same nested router, the Triton-shaped route keeps its
//! exact behaviour, and everything here is **opt-in** — without
//! [`SpecA2aConfig`] neither the JSON-RPC route nor the card exists, and
//! the surface is byte-for-byte what it was.
//!
//! Where the two differ, and why:
//!
//! * **Text, not `{tool, args}`.** A spec caller sends prose, so the text
//!   is dispatched to ONE configured tool (`default_tool`) as
//!   `{"message": <text>}` — the same convention the chat adapters use.
//!   Choosing the tool from caller-supplied data would make every
//!   registered tool remotely invocable by name.
//! * **Auth is unchanged.** The JSON-RPC route runs the same
//!   `IdentityProvider` as every other authenticated surface. The card is
//!   deliberately public (below).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::Dispatcher;

use crate::a2a::{A2aState, TaskState};

/// The A2A protocol version this facade implements.
const PROTOCOL_VERSION: &str = "0.3.0";

/// JSON-RPC error codes. The first four are standard JSON-RPC 2.0; the
/// `-32001` code is A2A's own `TaskNotFound`.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const TASK_NOT_FOUND: i32 = -32001;

/// What the Agent Card advertises, and which tool prose is dispatched to.
/// Supplied by the host (triton-embed's `EmbedOpts`), because only the
/// host knows its own public URL — the one thing a card cannot infer from
/// a request, since it is reached through an ingress that rewrites Host.
#[derive(Clone, Debug)]
pub struct SpecA2aConfig {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Public base URL, e.g. `https://agent-lab.data-zoo.de`. The card's
    /// `url` is this plus the A2A base path.
    pub public_url: String,
    /// Path the JSON-RPC endpoint is mounted at, relative to the host
    /// root (`/a2a` in the embedded host).
    pub a2a_path: String,
    /// The tool plain text is dispatched to.
    pub default_tool: String,
}

impl SpecA2aConfig {
    fn endpoint_url(&self) -> String {
        format!(
            "{}/{}",
            self.public_url.trim_end_matches('/'),
            self.a2a_path.trim_start_matches('/')
        )
    }
}

#[derive(Clone)]
pub struct CardState {
    pub config: Arc<SpecA2aConfig>,
    pub dispatcher: Arc<Dispatcher>,
    /// Every accepted `(issuer, audience)` pair, so the card can tell a
    /// caller which token to bring instead of leaving it to guess.
    pub oidc_providers: Vec<(String, String)>,
}

/// Every card filename spelling a discovery client is known to probe.
///
/// `agent-card.json` is the A2A spec's; `agent.json` is what Microsoft
/// Copilot Studio's own documentation tells operators to expect. The
/// remaining three (`agentcard.json`, `agentCard.json`,
/// `agent_card.json`) are Copilot Studio's live fallback probes —
/// observed 2026-08-30 in ingress logs, each returning 404 and adding
/// registration friction. Serving all five costs nothing and removes
/// every guess.
const CARD_FILENAMES: &[&str] = &[
    "agent-card.json",
    "agent.json",
    "agentcard.json",
    "agentCard.json",
    "agent_card.json",
];

/// The well-known card paths.
///
/// Mounted at the HOST ROOT (`<origin>/.well-known/...`) because that is
/// where browser-time discovery looks, AND — via [`card_router_nested`]
/// — under the A2A base (`<origin>/a2a/.well-known/...`) because Copilot
/// Studio's *runtime* orchestrator resolves the card RELATIVE TO THE
/// REGISTERED ENDPOINT (`.../a2a`), not the origin. Browser registration
/// succeeds off the root copy; the runtime call 404s without the nested
/// copy and fails Microsoft-side with a bare `SystemError` before any
/// request reaches us (confirmed 2026-08-30: zero POST /a2a during the
/// failing window).
pub fn card_router(state: CardState) -> Router {
    let mut r = Router::new();
    for name in CARD_FILENAMES {
        r = r.route(&format!("/.well-known/{name}"), get(agent_card));
    }
    r.with_state(state)
}

/// The same card routes, intended to be merged INTO the router that is
/// nested under `/a2a`, so they answer at `/a2a/.well-known/<name>`.
/// See [`card_router`] for why the endpoint-relative copy is required.
pub fn card_router_nested(state: CardState) -> Router {
    card_router(state)
}

/// The JSON-RPC route, to be merged into the nested A2A router so it
/// answers at the A2A base path itself.
pub fn jsonrpc_router(state: A2aState, config: Arc<SpecA2aConfig>) -> Router {
    Router::new()
        .route("/", post(jsonrpc))
        .with_state(SpecState { a2a: state, config })
}

#[derive(Clone)]
struct SpecState {
    a2a: A2aState,
    config: Arc<SpecA2aConfig>,
}

/// The Agent Card. **Unauthenticated on purpose**: discovery is what a
/// caller does BEFORE it has a credential, and the card's whole job is
/// to say which credential to bring. It contains only what is already
/// public — the agent's name, its endpoint, its tool names, and the
/// issuer/audience pairs it accepts (both are public identifiers that
/// already appear in PR diffs and browser URLs). No secret, and nothing
/// an unauthenticated caller could not learn by reading the deployment's
/// configuration.
async fn agent_card(State(state): State<CardState>) -> Response {
    let cfg = &state.config;

    // Skills come from the live tool registry rather than a hand-kept
    // list, so the card cannot drift from what the agent can actually do.
    let skills: Vec<Value> = state
        .dispatcher
        .descriptors()
        .into_iter()
        .map(|d| {
            json!({
                "id": d.name,
                "name": d.name,
                "description": format!("Triton tool `{}`", d.name),
                "tags": ["triton"],
            })
        })
        .collect();

    let mut card = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "name": cfg.name,
        "description": cfg.description,
        "url": cfg.endpoint_url(),
        "preferredTransport": "JSONRPC",
        "version": cfg.version,
        "capabilities": {
            // `message/stream` is implemented (#635 P6) — SSE of JSON-RPC
            // responses: Task(working) → artifact-updates → final
            // status-update. Flipped in the same commit that added the
            // method, never before.
            "streaming": true,
            "pushNotifications": false,
            // A2UI v0.9 (a2ui.org): the agent can return interactive UI
            // (cards, charts-as-images, buttons) as an `application/json+a2ui`
            // DataPart. Gemini Enterprise activates it via the
            // `X-A2A-Extensions` header and renders it from the basic catalog.
            "extensions": [{
                "uri": triton_core::a2ui::ge::EXTENSION_URI,
                "description": "Ability to render A2UI v0.9",
                "required": false,
                "params": {
                    "supportedCatalogIds": [triton_core::a2ui::ge::BASIC_CATALOG],
                    "acceptsInlineCatalogs": false,
                },
            }],
        },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": skills,
    });

    if !state.oidc_providers.is_empty() {
        card["securitySchemes"] = json!({
            "bearer": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT",
                "description": state
                    .oidc_providers
                    .iter()
                    .map(|(iss, aud)| format!("issuer {iss} / audience {aud}"))
                    .collect::<Vec<_>>()
                    .join("; or "),
            }
        });
        card["security"] = json!([{ "bearer": [] }]);
    }

    (StatusCode::OK, Json(card)).into_response()
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn rpc_error(id: &Value, code: i32, message: impl Into<String>) -> Response {
    // A JSON-RPC error is a 200 with an `error` member: the transport
    // succeeded, the call did not. The one exception below is auth,
    // where an HTTP status is what a caller's middleware acts on.
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() },
        })),
    )
        .into_response()
}

fn rpc_ok(id: &Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
        .into_response()
}

async fn jsonrpc(State(state): State<SpecState>, parts: Parts, body: Bytes) -> Response {
    // Auth first, exactly as the Triton-shaped route does: a malformed
    // body must not earn a different answer than a missing credential.
    let principal = match state.a2a.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.a2a.dispatcher.record_rejection(
                "message/send",
                "a2a",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            // 401, not a JSON-RPC error body: an unauthenticated caller
            // needs the HTTP status its client library already handles.
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": INVALID_REQUEST, "message": e.to_string() },
                })),
            )
                .into_response();
        }
    };

    // A2A extension activation: the client (e.g. Gemini Enterprise) lists the
    // extensions it activated in `X-A2A-Extensions`. Log it so we can see
    // whether GE actually activates A2UI, and echo the ones we support back on
    // the response (the A2A spec says the agent SHOULD confirm activation this
    // way — some renderers gate on the echo).
    let inbound_ext = parts
        .headers
        .get("x-a2a-extensions")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    tracing::info!(x_a2a_extensions = ?inbound_ext, "a2a inbound extension activation");
    let echo_a2ui = inbound_ext
        .as_deref()
        .map(|h| h.contains(triton_core::a2ui::ge::EXTENSION_URI))
        .unwrap_or(false);

    // DIAGNOSTIC: raw inbound body to stdout (captured in kubectl logs) —
    // to see exactly what Gemini Enterprise echoes on a card button click.
    {
        use std::io::Write as _;
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "A2A_INBOUND_BODY {}", String::from_utf8_lossy(&body));
        let _ = o.flush();
    }

    let req: RpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return rpc_error(&Value::Null, PARSE_ERROR, format!("invalid JSON: {e}")),
    };
    if req.jsonrpc.as_deref() != Some("2.0") {
        return rpc_error(&req.id, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }

    let mut resp = match req.method.as_str() {
        "message/send" => message_send(state, principal, req).await,
        "message/stream" => message_stream(state, principal, req).await,
        "tasks/get" => tasks_get(state, req),
        // Named explicitly so a caller learns which methods exist rather
        // than only that this one does not.
        other => rpc_error(
            &req.id,
            METHOD_NOT_FOUND,
            format!(
                "unsupported method `{other}`; this agent implements message/send, message/stream, tasks/get"
            ),
        ),
    };
    if echo_a2ui
        && let Ok(v) = axum::http::HeaderValue::from_str(triton_core::a2ui::ge::EXTENSION_URI)
    {
        resp.headers_mut().insert("x-a2a-extensions", v);
    }
    resp
}

/// Does the client want the Task back immediately instead of blocking
/// to a terminal state? The spec's `MessageSendConfiguration` has
/// carried this under two names across revisions (`blocking: false` in
/// v0.3, `returnImmediately: true` later); accept both — a consumer
/// pinned to either revision gets the behavior it asked for.
fn wants_immediate_task(params: &Value) -> bool {
    let Some(cfg) = params.get("configuration") else {
        return false;
    };
    cfg.get("blocking").and_then(Value::as_bool) == Some(false)
        || cfg.get("returnImmediately").and_then(Value::as_bool) == Some(true)
}

/// Concatenate every text part, which is how a multi-part user turn is
/// meant to read. Non-text parts (files, structured data) are ignored
/// rather than rejected: a client that also sends a file should still
/// get an answer to its words.
fn text_from_params(params: &Value) -> Option<String> {
    let parts = params.get("message")?.get("parts")?.as_array()?;
    // A2UI button click: Gemini Enterprise posts back the action as a
    // `application/json+a2ui` DataPart. Honour its resolved `question` (bound
    // by path in the card) as the turn text — this is what makes a follow-up
    // button re-ask instead of the agent seeing GE's "User action triggered."
    // placeholder text. Fall back to the plain text parts otherwise.
    if let Some(q) = a2ui_action_question(parts) {
        return Some(q);
    }
    let text = parts
        .iter()
        .filter(|p| p.get("kind").and_then(Value::as_str).unwrap_or("text") == "text")
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Pull a re-ask `question` out of an inbound A2UI `action` DataPart. The
/// client-to-server A2UI message rides `DataPart.data` (an object or a
/// one-element array) as `{version, action:{name, context:{question}}}`. We
/// look for a non-empty string `context.question`; absent ⇒ `None` (a normal
/// text turn). Tolerant of both the object and array `data` encodings.
fn a2ui_action_question(parts: &[Value]) -> Option<String> {
    for p in parts {
        if p.get("kind").and_then(Value::as_str) != Some("data") {
            continue;
        }
        let data = p.get("data")?;
        // `data` may be a single message object or an array of messages.
        let msgs: Vec<&Value> = match data {
            Value::Array(a) => a.iter().collect(),
            obj => vec![obj],
        };
        for m in msgs {
            let Some(action) = m.get("action") else {
                continue;
            };
            // Preferred: a resolved `context.question` (if GE ever forwards
            // context). Observed reality: GE posts `context:{}` and echoes the
            // event `name` verbatim, so the re-ask question rides the name as
            // `ask:<question>` (see triton_core::a2ui::ge). Decode either.
            if let Some(q) = action
                .get("context")
                .and_then(|c| c.get("question"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(q.to_string());
            }
            if let Some(q) = action
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| n.strip_prefix("ask:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(q.to_string());
            }
            // Reality on the GE wire: it echoes `sourceComponentId` (our button
            // id) but overwrites `name` and drops `context`. So the re-ask
            // question rides the button id as `ask-<hex>` (see
            // triton_core::a2ui::ge). Decode it.
            if let Some(q) = action
                .get("sourceComponentId")
                .and_then(Value::as_str)
                .and_then(|c| c.strip_prefix("ask-"))
                .and_then(triton_core::a2ui::ge::hex_decode)
                .map(|q| q.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                return Some(q);
            }
        }
    }
    None
}

async fn message_send(
    state: SpecState,
    principal: triton_core::Principal,
    req: RpcRequest,
) -> Response {
    let Some(text) = text_from_params(&req.params) else {
        return rpc_error(
            &req.id,
            INVALID_PARAMS,
            "params.message.parts must contain at least one non-empty text part",
        );
    };
    // A2A 0.3.0 requires `contextId` on Task; honor a client-supplied
    // `message.contextId`, else generate one (strict clients reject its
    // absence). Also stamped on the returned Message for follow-ups.
    let context_id = req
        .params
        .get("message")
        .and_then(|m| m.get("contextId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let trace_id = principal.trace_id.clone();
    let tool = state.config.default_tool.clone();

    // #635 P6 — disconnect-safe by construction: the dispatch runs in a
    // SPAWNED task that records its terminal state (and the clamped
    // reply) into the store from inside itself. The handler merely
    // awaits the JoinHandle, so a client hanging up mid-turn drops the
    // await, never the dispatch — the answer completes and `tasks/get`
    // returns it. This is the same bug class as the Teams webhook 499
    // (a 23s answer computed and lost to a cancelled future), fixed the
    // same way.
    state.a2a.tasks.record(&trace_id, TaskState::Working);
    let dispatcher = state.a2a.dispatcher.clone();
    let tasks = state.a2a.tasks.clone();
    let task_trace = trace_id.clone();
    let handle = tokio::spawn(async move {
        let out = dispatcher
            .invoke(&tool, json!({ "message": text }), principal, "a2a")
            .await;
        match &out {
            Ok(d) => tasks.record_entry(
                &task_trace,
                TaskState::Completed,
                Some(&reply_text(&d.result)),
                None,
            ),
            Err(e) => {
                tasks.record_entry(&task_trace, TaskState::Failed, None, Some(&e.to_string()));
            }
        }
        out
    });

    if wants_immediate_task(&req.params) {
        // The spec: return the Task right away; the client polls
        // `tasks/get`. The spawned dispatch keeps running.
        let task = json!({
            "kind": "task",
            "id": trace_id,
            "contextId": context_id,
            "status": { "state": "working" },
        });
        return rpc_ok(&req.id, task);
    }

    match handle.await {
        Ok(Ok(d)) => {
            let reply = reply_text(&d.result);
            let mut parts = vec![json!({ "kind": "text", "text": reply })];
            if let Some(msgs) = triton_core::a2ui::ge::build_messages(&d.result) {
                parts.extend(triton_core::a2ui::ge::data_parts(msgs));
            }
            let msg = json!({
                "kind": "message",
                "role": "agent",
                "messageId": uuid::Uuid::new_v4().to_string(),
                "parts": parts,
                // The task id, so a caller can follow up via tasks/get.
                "taskId": d.trace_id,
                "contextId": context_id,
            });
            rpc_ok(&req.id, msg)
        }
        Ok(Err(e)) => rpc_error(&req.id, INTERNAL_ERROR, e.to_string()),
        Err(join_err) => rpc_error(
            &req.id,
            INTERNAL_ERROR,
            format!("dispatch task join error: {join_err}"),
        ),
    }
}

/// Spec `message/stream` (#635 P6): SSE of JSON-RPC responses — the
/// initial Task (`working`), a `TaskArtifactUpdateEvent` per token
/// delta (append: true; from the in-process streaming seam when the
/// tool opts in, else nothing until the end), the full reply as a
/// last-chunk artifact, and a terminal `TaskStatusUpdateEvent`
/// (`final: true`, closing the stream per spec). Task state is
/// recorded exactly as message/send records it, so `tasks/get` works
/// on streamed turns too.
async fn message_stream(
    state: SpecState,
    principal: triton_core::Principal,
    req: RpcRequest,
) -> Response {
    use futures::StreamExt as _;

    let Some(text) = text_from_params(&req.params) else {
        return rpc_error(
            &req.id,
            INVALID_PARAMS,
            "params.message.parts must contain at least one non-empty text part",
        );
    };
    let trace_id = principal.trace_id.clone();
    let tool = state.config.default_tool.clone();
    let rpc_id = req.id.clone();

    // A2A 0.3.0 requires `contextId` on Task / TaskStatusUpdateEvent /
    // TaskArtifactUpdateEvent. Strict clients (Gemini Enterprise's a2a-python
    // SDK) reject the whole stream if any frame omits it. Honor a
    // client-supplied `message.contextId`; otherwise generate one and reuse it
    // on every frame so the turn's events share one context.
    let context_id = req
        .params
        .get("message")
        .and_then(|m| m.get("contextId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    state.a2a.tasks.record(&trace_id, TaskState::Working);
    let events = match state
        .a2a
        .dispatcher
        .invoke_streaming(&tool, json!({ "message": text }), principal, "a2a", None)
        .await
    {
        Ok(ev) => ev,
        Err(e) => {
            state
                .a2a
                .tasks
                .record_entry(&trace_id, TaskState::Failed, None, Some(&e.to_string()));
            return rpc_error(&req.id, INTERNAL_ERROR, e.to_string());
        }
    };

    let tasks = state.a2a.tasks.clone();
    let task_id = trace_id.clone();
    let rpc = move |result: Value| serde_json::json!({ "jsonrpc": "2.0", "id": rpc_id.clone(), "result": result });
    let initial = rpc(json!({
        "kind": "task",
        "id": task_id,
        "contextId": context_id,
        "status": { "state": "working" },
    }));

    let artifact_id = uuid::Uuid::new_v4().to_string();
    let task_for_frames = trace_id.clone();
    let ctx_for_frames = context_id.clone();
    let rpc_frames = rpc.clone();
    // Whether we streamed the answer as token deltas. If we did, the final
    // artifact must NOT resend the full reply text — the client accumulates
    // append-chunks, so a second full copy shows the answer twice (and drags
    // the trailing `![chart]` markdown in as literal text on hosts that render
    // A2UI, like Gemini Enterprise). When nothing streamed (a buffered tool),
    // the final artifact carries the text once.
    let mut streamed_text = false;
    let frames = events.flat_map(move |ev| {
        let out: Vec<Value> = match ev {
            triton_core::stream::StreamEvent::Token(t) => {
                streamed_text = true;
                vec![rpc_frames(json!({
                    "kind": "artifact-update",
                    "taskId": task_for_frames,
                    "contextId": ctx_for_frames,
                    "append": true,
                    "artifact": {
                        "artifactId": artifact_id,
                        "parts": [{ "kind": "text", "text": t }],
                    },
                }))]
            }
            triton_core::stream::StreamEvent::Tool(_) => Vec::new(),
            triton_core::stream::StreamEvent::Done(v) => {
                let reply = reply_text(&v);
                tasks.record_entry(&task_for_frames, TaskState::Completed, Some(&reply), None);
                // Final answer artifact carries the text part (every client)
                // AND, when the surface has renderable components, an A2UI
                // v0.9 DataPart (Gemini Enterprise renders the card/chart/
                // buttons; text-only clients ignore the data part).
                // Only include the full text if we did NOT stream it as
                // deltas (else it duplicates). A2UI DataParts always ride the
                // final artifact (deltas never carried them).
                let mut parts: Vec<Value> = Vec::new();
                if !streamed_text {
                    parts.push(json!({ "kind": "text", "text": reply }));
                }
                if let Some(msgs) = triton_core::a2ui::ge::build_messages(&v) {
                    parts.extend(triton_core::a2ui::ge::data_parts(msgs));
                }
                let mut frames = Vec::new();
                if !parts.is_empty() {
                    frames.push(rpc_frames(json!({
                        "kind": "artifact-update",
                        "taskId": task_for_frames,
                        "contextId": ctx_for_frames,
                        "lastChunk": true,
                        "artifact": {
                            "artifactId": artifact_id,
                            "parts": parts,
                        },
                    })));
                }
                frames.push(rpc_frames(json!({
                    "kind": "status-update",
                    "taskId": task_for_frames,
                    "contextId": ctx_for_frames,
                    "status": { "state": "completed" },
                    "final": true,
                })));
                frames
            }
            triton_core::stream::StreamEvent::Error { error, .. } => {
                tasks.record_entry(
                    &task_for_frames,
                    TaskState::Failed,
                    None,
                    Some(&error.to_string()),
                );
                vec![rpc_frames(json!({
                    "kind": "status-update",
                    "taskId": task_for_frames,
                    "contextId": ctx_for_frames,
                    "status": { "state": "failed", "message": {
                        "kind": "message", "role": "agent",
                        "messageId": uuid::Uuid::new_v4().to_string(),
                        "parts": [{ "kind": "text", "text": error.to_string() }],
                    } },
                    "final": true,
                }))]
            }
        };
        futures::stream::iter(out)
    });

    let all = futures::stream::once(async move { initial }).chain(frames);
    let sse = all.map(|v| {
        Ok::<axum::response::sse::Event, std::convert::Infallible>(
            axum::response::sse::Event::default().data(v.to_string()),
        )
    });
    axum::response::Sse::new(sse)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
        )
        .into_response()
}

/// Pull human-readable text out of whatever the tool returned. Triton
/// tools answer with an A2UI surface; a spec-A2A caller asked for
/// `text/plain`, so the surface's text is what it gets, and the whole
/// JSON only as a last resort (better than an empty reply).
fn reply_text(result: &Value) -> String {
    for key in ["text", "message", "answer"] {
        if let Some(s) = result.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    if let Some(surface) = result.get("surface") {
        if let Some(s) = surface.get("text").and_then(Value::as_str) {
            return s.to_string();
        }
        // The embedded agent's real shape: `surface.components`, where
        // prose rides as `text`/`narration` components (`value`/`text`
        // fields). Without this, live A2A callers got the whole result
        // JSON — tool_trace and all — as their "answer" (#635 E2E).
        if let Some(components) = surface.get("components").and_then(Value::as_array) {
            let mut joined = components
                .iter()
                .filter_map(
                    |c| match c.get("kind").and_then(Value::as_str).unwrap_or_default() {
                        "text" => c.get("value").and_then(Value::as_str),
                        "narration" => c.get("text").and_then(Value::as_str),
                        _ => None,
                    },
                )
                .collect::<Vec<_>>()
                .join("\n");
            // Charts: image-hosting chat surfaces expand a `report`
            // component into a card image; a text/Markdown surface (A2A →
            // Copilot Studio, Gemini) can't, so without this the caller
            // got prose only and the model drew ASCII "charts". When the
            // producer stamped a public `image_url` on the report (the
            // embedded agent's signed /report/img route), render it as a
            // Markdown image — Copilot Studio and Gemini display it inline.
            let images = components
                .iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) == Some("report"))
                .filter_map(|c| c.get("image_url").and_then(Value::as_str))
                .map(|u| format!("![chart]({u})"))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !images.is_empty() {
                if !joined.is_empty() {
                    joined.push_str("\n\n");
                }
                joined.push_str(&images);
            }
            if !joined.is_empty() {
                return joined;
            }
        }
        if let Some(items) = surface.get("items").and_then(Value::as_array) {
            let joined = items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return joined;
            }
        }
    }
    result.to_string()
}

fn tasks_get(state: SpecState, req: RpcRequest) -> Response {
    let Some(id) = req.params.get("id").and_then(Value::as_str) else {
        return rpc_error(&req.id, INVALID_PARAMS, "params.id is required");
    };
    match state.a2a.tasks.entry(id) {
        Some(entry) => {
            let st = entry.state.unwrap_or(TaskState::Submitted);
            let mut task = json!({
                "kind": "task",
                "id": id,
                "status": { "state": match st {
                    TaskState::Submitted => "submitted",
                    TaskState::Working => "working",
                    TaskState::Completed => "completed",
                    TaskState::Failed => "failed",
                } },
            });
            // Completed: the stored (clamped) reply rides as an
            // artifact — this is what makes a disconnected
            // `message/send` recoverable by polling.
            if st == TaskState::Completed
                && let Some(result) = &entry.result
            {
                task["artifacts"] = json!([{
                    "artifactId": uuid::Uuid::new_v4().to_string(),
                    "parts": [{ "kind": "text", "text": result }],
                }]);
            }
            if st == TaskState::Failed
                && let Some(error) = &entry.error
            {
                task["status"]["message"] = json!({
                    "kind": "message", "role": "agent",
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "parts": [{ "kind": "text", "text": error }],
                });
            }
            rpc_ok(&req.id, task)
        }
        // The store is bounded and restart-clean, so "not found" also
        // covers "evicted" and "from a previous process". A caller
        // cannot distinguish those, and the spec has one code for it.
        None => rpc_error(&req.id, TASK_NOT_FOUND, format!("no task `{id}`")),
    }
}

/// Re-exported for the host so it can build [`CardState`] without
/// depending on this module's internals.
pub fn card_state(
    config: SpecA2aConfig,
    dispatcher: Arc<Dispatcher>,
    oidc_providers: Vec<(String, String)>,
) -> CardState {
    CardState {
        config: Arc::new(config),
        dispatcher,
        oidc_providers,
    }
}

#[cfg(test)]
mod reply_text_tests {
    use super::*;

    /// #635 E2E finding: the embedded agent's result is
    /// `surface.components`; without component extraction, A2A callers
    /// received the whole result JSON (tool_trace included) as prose.
    #[test]
    fn reply_text_reads_surface_components() {
        let result = serde_json::json!({
            "_meta": { "tool_trace": [ { "tool": "run_query" } ] },
            "surface": { "components": [
                { "kind": "text", "value": "Initech leads at $2,500.75." },
                { "kind": "button", "label": "Details" },
                { "kind": "narration", "text": "figures from top_customers" }
            ] }
        });
        let text = reply_text(&result);
        assert_eq!(
            text,
            "Initech leads at $2,500.75.\nfigures from top_customers"
        );
        assert!(!text.contains("tool_trace"));
    }

    /// A `report` component carrying a producer-stamped `image_url` is
    /// rendered as a trailing Markdown image so A2A/Copilot show the real
    /// chart instead of the model's ASCII fallback. A report WITHOUT a
    /// url (unconfigured producer) adds nothing.
    #[test]
    fn reply_text_appends_report_image_as_markdown() {
        let result = serde_json::json!({
            "surface": { "components": [
                { "kind": "text", "value": "US leads at $4,000.75." },
                { "kind": "report", "report_id": "sales_by_region",
                  "image_url": "https://agent.example/report/img/TOKEN" },
                { "kind": "button", "label": "Open report" }
            ] }
        });
        assert_eq!(
            reply_text(&result),
            "US leads at $4,000.75.\n\n![chart](https://agent.example/report/img/TOKEN)"
        );

        let no_url = serde_json::json!({
            "surface": { "components": [
                { "kind": "text", "value": "US leads at $4,000.75." },
                { "kind": "report", "report_id": "sales_by_region" }
            ] }
        });
        assert_eq!(reply_text(&no_url), "US leads at $4,000.75.");
    }
}
