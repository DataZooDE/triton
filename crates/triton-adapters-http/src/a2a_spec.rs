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

/// The two well-known card paths, mounted at the HOST ROOT (not under
/// the A2A base) because that is where discovery looks.
///
/// Both spellings are served on purpose: `agent-card.json` is the
/// spec's, and `agent.json` is what Microsoft Copilot Studio's own
/// documentation tells operators to expect. Serving one and guessing
/// right is not better than serving both.
pub fn card_router(state: CardState) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(agent_card))
        .route("/.well-known/agent.json", get(agent_card))
        .with_state(state)
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
            // Streaming is the Triton-shaped route's `Accept:
            // text/event-stream` mode, which is NOT the spec's
            // `message/stream` method — claiming it here would make a
            // conformant client call a method that does not exist.
            "streaming": false,
            "pushNotifications": false,
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

    let req: RpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return rpc_error(&Value::Null, PARSE_ERROR, format!("invalid JSON: {e}")),
    };
    if req.jsonrpc.as_deref() != Some("2.0") {
        return rpc_error(&req.id, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }

    match req.method.as_str() {
        "message/send" => message_send(state, principal, req).await,
        "tasks/get" => tasks_get(state, req),
        // Named explicitly so a caller learns which methods exist rather
        // than only that this one does not.
        other => rpc_error(
            &req.id,
            METHOD_NOT_FOUND,
            format!("unsupported method `{other}`; this agent implements message/send, tasks/get"),
        ),
    }
}

/// Concatenate every text part, which is how a multi-part user turn is
/// meant to read. Non-text parts (files, structured data) are ignored
/// rather than rejected: a client that also sends a file should still
/// get an answer to its words.
fn text_from_params(params: &Value) -> Option<String> {
    let parts = params.get("message")?.get("parts")?.as_array()?;
    let text = parts
        .iter()
        .filter(|p| p.get("kind").and_then(Value::as_str).unwrap_or("text") == "text")
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
    let context_id = req
        .params
        .get("message")
        .and_then(|m| m.get("contextId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let trace_id = principal.trace_id.clone();
    let tool = state.config.default_tool.clone();

    match state
        .a2a
        .dispatcher
        .invoke(&tool, json!({ "message": text }), principal, "a2a")
        .await
    {
        Ok(d) => {
            state.a2a.tasks.record(&trace_id, TaskState::Completed);
            let reply = reply_text(&d.result);
            let mut msg = json!({
                "kind": "message",
                "role": "agent",
                "messageId": uuid::Uuid::new_v4().to_string(),
                "parts": [{ "kind": "text", "text": reply }],
                // The task id, so a caller can follow up via tasks/get.
                "taskId": d.trace_id,
            });
            if let Some(ctx) = context_id {
                msg["contextId"] = json!(ctx);
            }
            rpc_ok(&req.id, msg)
        }
        Err(e) => {
            state.a2a.tasks.record(&trace_id, TaskState::Failed);
            rpc_error(&req.id, INTERNAL_ERROR, e.to_string())
        }
    }
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
    match state.a2a.tasks.get(id) {
        Some(st) => rpc_ok(
            &req.id,
            json!({
                "kind": "task",
                "id": id,
                "status": { "state": match st {
                    TaskState::Completed => "completed",
                    TaskState::Failed => "failed",
                } },
            }),
        ),
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
