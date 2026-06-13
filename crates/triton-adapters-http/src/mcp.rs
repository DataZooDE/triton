//! MCP adapter — hand-rolled JSON-RPC over axum (ADR-2, FR-A-6).
//!
//! Per realization §2: stay with serde_json + axum; don't pull in
//! `rmcp`/`tonic` for a thin layer. Plain JSON responses (no SSE).
//! `Arc<Mutex<HashSet<String>>>` for session tracking is overkill
//! for our stateless surface — we initialise to an empty set and
//! never branch on it, but the structure is here for the day MCP
//! grows a session-bound capability.
//!
//! Methods implemented (FR-A-6):
//!   * `initialize`     — handshake; advertises tools capability.
//!   * `tools/list`     — registry descriptors in MCP shape (camelCase).
//!   * `tools/call`     — dispatch through `Dispatcher`.
//!   * `resources/read` — stub for `ui://triton/runtime.html`.
//!
//! ADR-6: this module is a thin unwrap/wrap shell. The dispatcher
//! owns audit emission; the adapter calls `record_rejection` on
//! boundary failures so the FR-AU-1 guarantee holds.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use triton_core::a2ui::{build_envelope, extract_surface};
use triton_core::{A2uiVersion, Dispatcher, TritonError, envelope};

use crate::identity::IdentityProvider;

/// JSON-RPC 2.0 error codes per architecture.md §8.3.
const CODE_PARSE_ERROR: i32 = -32700;
const CODE_INVALID_REQUEST: i32 = -32600;
const CODE_METHOD_NOT_FOUND: i32 = -32601;
const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_AUTH: i32 = -32001;
const CODE_TOOL_PROVIDER: i32 = -32000;
/// PR 24: dedicated MCP code for rate-limit refusals so clients
/// can distinguish backoff-worthy throttling from a tool/provider
/// failure (Codex PR 24 concern). JSON-RPC reserves -32768..-32000
/// for implementation-defined server errors; we pick -32002 to
/// neighbour CODE_AUTH and stay outside the reserved set.
const CODE_RATE_LIMITED: i32 = -32002;

/// MCP protocol versions Triton speaks. Echoing a client-provided
/// version we don't actually support would be a quiet lie; reject
/// unknowns with `-32602` so the client downgrades or fails fast.
const SUPPORTED_MCP_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Session ID registry. Stays in process memory (G-8); restart-clean
/// by construction. PR 7 doesn't yet use the set — it's wired so a
/// future PR can plug in MCP's session-bound capabilities without
/// touching the public adapter API.
#[derive(Clone, Default)]
pub struct McpSessions {
    #[allow(dead_code)]
    ids: Arc<Mutex<HashSet<String>>>,
}

impl McpSessions {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Clone)]
pub struct McpState {
    pub dispatcher: Arc<Dispatcher>,
    pub sessions: McpSessions,
    pub identity: Arc<IdentityProvider>,
}

pub fn router(state: McpState) -> Router {
    Router::new().route("/", post(rpc)).with_state(state)
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

async fn rpc(State(state): State<McpState>, parts: Parts, body: Bytes) -> Response {
    // Step 1: parse the body as raw JSON. A malformed body is the
    // only path that yields -32700; anything that parses as JSON
    // but is shape-wrong gets -32600 below.
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = TritonError::Validation(format!("invalid JSON: {e}"));
            state.dispatcher.record_rejection(
                "rpc",
                "mcp",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &err,
            );
            return rpc_error(Value::Null, CODE_PARSE_ERROR, &err.to_string());
        }
    };

    // Step 2: envelope-shape checks (jsonrpc, method, batch vs object).
    // Failures here are -32600 Invalid Request, NOT -32700.
    let Some(obj) = raw.as_object() else {
        let err = TritonError::Validation(
            "JSON-RPC request MUST be an object (batch unsupported)".into(),
        );
        record_rejection(&state, "rpc", &err);
        return rpc_error(Value::Null, CODE_INVALID_REQUEST, &err.to_string());
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let err = TritonError::Validation("jsonrpc field MUST be \"2.0\"".into());
        record_rejection(&state, "rpc", &err);
        return rpc_error(
            obj.get("id").cloned().unwrap_or(Value::Null),
            CODE_INVALID_REQUEST,
            &err.to_string(),
        );
    }
    let Some(method) = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let err = TritonError::Validation("method field MUST be a string".into());
        record_rejection(&state, "rpc", &err);
        return rpc_error(
            obj.get("id").cloned().unwrap_or(Value::Null),
            CODE_INVALID_REQUEST,
            &err.to_string(),
        );
    };

    // Step 3: detect notifications. A request without `id` is a
    // notification per JSON-RPC 2.0; we MUST NOT respond. We still
    // run auth + dispatch so the audit log records the call.
    let id_present = obj.contains_key("id");
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    // Step 4: auth applies to every method. `initialize` could in
    // principle be unauthenticated, but the substrate ACL already
    // protects the MCP port (architecture §7) so we keep the same
    // identity check across all methods for symmetry with REST/A2A.
    let principal = match state.identity.verify(&parts).await {
        Ok(p) => p,
        Err(e) => {
            state.dispatcher.record_rejection(
                &method,
                "mcp",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            return maybe_respond(id_present, id, |id| {
                rpc_error(id, CODE_AUTH, &e.to_string())
            });
        }
    };

    // Step 5: method dispatch.
    let resp_factory: Box<dyn FnOnce(Value) -> Response> = match method.as_str() {
        "initialize" => Box::new(move |id| initialize_response(id, &params, &state)),
        "tools/list" => {
            let response = tools_list_response(id.clone(), &state).await;
            return maybe_respond(id_present, id, |_| response);
        }
        "tools/call" => {
            let response = tools_call(id.clone(), params, principal, &state).await;
            return maybe_respond(id_present, id, |_| response);
        }
        "resources/read" => Box::new(move |id| resources_read(id, &params)),
        other => {
            let err = TritonError::Validation(format!("unknown method: {other}"));
            state.dispatcher.record_rejection(
                other,
                "mcp",
                &principal.sub,
                &principal.tenant,
                &principal.trace_id,
                &err,
            );
            let msg = err.to_string();
            Box::new(move |id| rpc_error(id, CODE_METHOD_NOT_FOUND, &msg))
        }
    };
    maybe_respond(id_present, id, resp_factory)
}

/// If `id` was present, return the response. Otherwise (notification)
/// return an empty `200 OK` per JSON-RPC 2.0.
fn maybe_respond<F>(id_present: bool, id: Value, build: F) -> Response
where
    F: FnOnce(Value) -> Response,
{
    if id_present {
        build(id)
    } else {
        StatusCode::OK.into_response()
    }
}

fn record_rejection(state: &McpState, tool: &str, err: &TritonError) {
    state.dispatcher.record_rejection(
        tool,
        "mcp",
        "-",
        "-",
        &uuid::Uuid::new_v4().to_string(),
        err,
    );
}

fn initialize_response(id: Value, params: &Value, _state: &McpState) -> Response {
    // FR-A-6: emit the version associated with the negotiated MCP
    // App. Reject unknown versions instead of silently echoing
    // back — Codex PR 7 finding.
    let requested = params["protocolVersion"].as_str();
    let version = match requested {
        Some(v) if SUPPORTED_MCP_VERSIONS.contains(&v) => v.to_string(),
        Some(other) => {
            return rpc_error(
                id,
                CODE_INVALID_PARAMS,
                &format!(
                    "unsupported protocolVersion: {other} (supported: {})",
                    SUPPORTED_MCP_VERSIONS.join(", ")
                ),
            );
        }
        None => SUPPORTED_MCP_VERSIONS
            .last()
            .copied()
            .unwrap_or("2025-06-18")
            .to_string(),
    };
    rpc_ok(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "triton",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    )
}

async fn tools_list_response(id: Value, state: &McpState) -> Response {
    // MCP's tool descriptor uses camelCase (`inputSchema`) — we
    // translate from `ToolDescriptor` here. `returns_a2ui` and the
    // upstream flag are Triton-specific and not part of MCP's spec, so
    // we expose them under a vendor extension `x-triton`.
    let tools: Vec<Value> = state
        .dispatcher
        .descriptors_all()
        .await
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "inputSchema": d.input_schema,
                "x-triton": { "returns_a2ui": d.returns_a2ui, "upstream": d.upstream },
            })
        })
        .collect();
    rpc_ok(id, json!({ "tools": tools }))
}

async fn tools_call(
    id: Value,
    params: Value,
    principal: triton_core::Principal,
    state: &McpState,
) -> Response {
    let Some(tool_name) = params["name"].as_str() else {
        return rpc_error(id, CODE_INVALID_PARAMS, "params.name MUST be a string");
    };
    let tool_name = tool_name.to_string();
    let args = params["arguments"].clone();

    // FR-A-3 / FR-A-6: MCP's per-call A2UI version comes in
    // `params._meta.a2ui_version`. Unknown values are rejected the
    // same way A2A handles `metadata.a2ui_version` so the negotiated
    // version is never silently downgraded.
    let requested = match parse_mcp_a2ui_version(&params) {
        Ok(v) => v,
        Err(e) => return rpc_error(id, CODE_INVALID_PARAMS, &e),
    };

    match state
        .dispatcher
        .invoke(&tool_name, args, principal, "mcp")
        .await
    {
        Ok(mut d) => {
            let trace_id = d.trace_id.clone();
            if let Err(e) = wrap_a2ui_if_requested(&mut d, requested) {
                return rpc_error(id, code_for(&e), &e.to_string());
            }
            // MCP's `tools/call` result carries `content` (text-shaped
            // for UI display) and `structuredContent` (typed value).
            // We serialise the canonical envelope into both so a
            // text-only MCP client sees something sensible and a
            // structured client gets the same dict REST/A2A get.
            let env_v = envelope(&d);
            rpc_ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": env_v.to_string() }],
                    "structuredContent": env_v,
                    "isError": false,
                    "_meta": { "trace_id": trace_id }
                }),
            )
        }
        Err(e) => rpc_error(id, code_for(&e), &e.to_string()),
    }
}

fn resources_read(id: Value, params: &Value) -> Response {
    let uri = params["uri"].as_str().unwrap_or("");
    match uri {
        "ui://triton/runtime.html" => rpc_ok(
            id,
            json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/html",
                    // Stub per FR-A-6 — production deployments do
                    // not serve the Lit runtime; the resource exists
                    // for MCP-spec compliance only.
                    "text": "<!-- triton runtime stub; see realizations.md §1 -->"
                }]
            }),
        ),
        other => rpc_error(
            id,
            CODE_INVALID_PARAMS,
            &format!("unknown resource uri: {other}"),
        ),
    }
}

fn parse_mcp_a2ui_version(params: &Value) -> Result<Option<A2uiVersion>, String> {
    let Some(v) = params["_meta"]["a2ui_version"].as_str() else {
        return Ok(None);
    };
    match v {
        "v0.8" => Ok(Some(A2uiVersion::V08)),
        "v0.9" => Ok(Some(A2uiVersion::V09)),
        other => Err(format!("unknown A2UI version: {other}")),
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

fn code_for(e: &TritonError) -> i32 {
    match e {
        TritonError::Auth(_) | TritonError::Forbidden(_) => CODE_AUTH,
        TritonError::Validation(_) => CODE_INVALID_PARAMS,
        TritonError::Tool(_) | TritonError::Provider(_) => CODE_TOOL_PROVIDER,
        TritonError::RateLimited(_) => CODE_RATE_LIMITED,
    }
}

fn rpc_ok(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }),
    )
        .into_response()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.to_string(),
            }),
        }),
    )
        .into_response()
}
