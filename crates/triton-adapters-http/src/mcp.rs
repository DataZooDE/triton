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
//!   * `server/discover` — MCP 2026-07-28 stateless discovery.
//!   * `initialize`     — handshake; advertises tools capability.
//!   * `tools/list`     — registry descriptors in MCP shape (camelCase).
//!   * `tools/call`     — dispatch through `Dispatcher`.
//!   * `resources/read` — stub for `ui://triton/runtime.html`.
//!
//! Both protocol eras share the endpoint: a request carrying the
//! 2026-07-28 `_meta` envelope is served statelessly (results gain
//! `resultType`, lists and resource reads gain cache hints, an unknown
//! method is `404`); anything else is a handshake-era client.
//! Notifications are `202 Accepted` with no body.
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
use triton_core::a2ui::{build_envelope, extract_surface, mcp_app};
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
/// Handshake-era (`initialize`) revisions, oldest first.
const SUPPORTED_MCP_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Stateless revisions (per-request `_meta`, `server/discover`, no
/// `initialize`), newest first.
const MODERN_MCP_VERSIONS: &[&str] = &["2026-07-28"];

/// `_meta` key carrying a 2026-07-28 request's protocol version.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// MCP 2026-07-28 protocol error codes (schema `HeaderMismatchError`,
/// `UnsupportedProtocolVersionError`).
const CODE_HEADER_MISMATCH: i32 = -32020;
const CODE_UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// Cache hint on `server/discover`: the answer changes only when Triton
/// is upgraded.
const DISCOVER_TTL_MS: u64 = 300_000;

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
    /// When set, an upstream tool call runs as a task, so an
    /// upstream that goes asynchronous gets a "filed" answer
    /// (`check_after_seconds`) instead of holding the call open.
    pub tasks: Option<crate::tasks::Runner>,
    /// Set on a customer path (`/mcp/<slug>`, [`crate::mcp_tenants`]): the
    /// verified principal must also pass the route's tenant binding.
    pub route: Option<Arc<crate::mcp_tenants::McpRoute>>,
    /// Serve the answer-card MCP Apps widget ([`mcp_app`]): its resource,
    /// the tool `_meta` that names it, and its data on each result. A host
    /// without MCP Apps ignores all three; `TRITON_MCP_APP_WIDGET=off`
    /// switches it off for a host that renders it badly.
    pub widget: bool,
}

pub fn router(state: McpState) -> Router {
    Router::new()
        .route("/", post(rpc))
        .with_state(state)
        .layer(axum::middleware::from_fn(crate::identity::bearer_challenge))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

pub(crate) async fn rpc(State(state): State<McpState>, parts: Parts, body: Bytes) -> Response {
    // Step 1: parse the body as raw JSON. The verdict is held until the
    // caller is authenticated: an unauthenticated POST — even an empty
    // one, which connectors send to find the authorization server — must
    // get the 401 challenge, not a parse error.
    let parsed: Result<Value, String> = serde_json::from_slice(&body).map_err(|e| e.to_string());
    let method_hint = parsed
        .as_ref()
        .ok()
        .and_then(|v| v.get("method"))
        .and_then(Value::as_str)
        .unwrap_or("rpc")
        .to_string();

    // Step 2: auth applies to every method. `initialize` could in
    // principle be unauthenticated, but the substrate ACL already
    // protects the MCP port (architecture §7) so we keep the same
    // identity check across all methods for symmetry with REST/A2A.
    let verified = match (state.identity.verify(&parts).await, &state.route) {
        (Ok(p), Some(route)) => route.admit(&p).map(|()| p),
        (verified, _) => verified,
    };
    let principal = match verified {
        Ok(p) => p,
        Err(e) => {
            // The audit line names only the method; the client and the uri
            // tell a refused host fetch (a widget template) from a probe.
            let uri = parsed
                .as_ref()
                .ok()
                .and_then(|v| v.pointer("/params/uri"))
                .and_then(|u| u.as_str())
                .unwrap_or("");
            tracing::info!(
                method = %method_hint,
                uri,
                slug = state.route.as_ref().map(|r| r.slug.as_str()).unwrap_or(""),
                client = user_agent(&parts),
                error = %e,
                "mcp request refused"
            );
            state.dispatcher.record_rejection(
                &method_hint,
                "mcp",
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &e,
            );
            let id = parsed
                .as_ref()
                .ok()
                .and_then(|v| v.get("id"))
                .cloned()
                .unwrap_or(Value::Null);
            // HTTP 401, not a 200 carrying the error: MCP's HTTP
            // authorization makes a 401 (with `WWW-Authenticate`, added by
            // the router's `bearer_challenge` layer) the signal on which a
            // client starts its OAuth flow. The JSON-RPC body is kept.
            let mut response = rpc_error(id, CODE_AUTH, &e.to_string());
            // A missing delegated scope answers 403 (RFC 6750 §3.1
            // `insufficient_scope`, ADR-22); every other failure is 401.
            *response.status_mut() = match e {
                TritonError::Forbidden(_) => StatusCode::FORBIDDEN,
                _ => StatusCode::UNAUTHORIZED,
            };
            return state.identity.tag_rejection(&e, response);
        }
    };

    // Step 3: a body that is not JSON is -32700 with `400 Bad Request`
    // (Streamable HTTP: a POST body MUST be one JSON-RPC message).
    let raw = match parsed {
        Ok(v) => v,
        Err(e) => {
            let err = TritonError::Validation(format!("invalid JSON: {e}"));
            record_rejection(&state, "rpc", &err);
            return with_status(
                rpc_error(Value::Null, CODE_PARSE_ERROR, &err.to_string()),
                StatusCode::BAD_REQUEST,
            );
        }
    };

    // Step 4: envelope-shape checks (jsonrpc, method, batch vs object).
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

    // Step 5: detect notifications. A message without `id` is a
    // notification per JSON-RPC 2.0; it gets no JSON-RPC response, and
    // Streamable HTTP answers an accepted one `202 Accepted` with no body.
    let id_present = obj.contains_key("id");
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    // Step 6: protocol era. A request carrying the 2026-07-28 envelope
    // (`_meta` protocol version, or an `MCP-Protocol-Version` header naming
    // no handshake-era revision) is served statelessly per that revision;
    // anything else is a handshake-era (`initialize`) client.
    let modern = match era(&parts, &params, &method) {
        Ok(modern) => modern,
        Err((code, message, data)) => {
            let err = TritonError::Validation(message.clone());
            state.dispatcher.record_rejection(
                &method,
                "mcp",
                &principal.sub,
                &principal.tenant,
                &principal.trace_id,
                &err,
            );
            return with_status(
                rpc_error_with_data(id, code, &message, data),
                StatusCode::BAD_REQUEST,
            );
        }
    };

    // Step 7: client notifications (`notifications/initialized`,
    // `notifications/cancelled`, …) carry nothing Triton acts on.
    if !id_present && method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }

    // Step 8: method dispatch. Calls are audited by the dispatcher; the
    // rest (lists, resource reads, discovery) is not a dispatch, and hosts
    // send those out of band (ChatGPT fetches a widget at "Refresh tools"),
    // so each is logged: whether a host ever asked is the first question
    // when a widget does not open.
    if method != "tools/call" {
        let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        let slug = state.route.as_ref().map(|r| r.slug.as_str()).unwrap_or("");
        let client = user_agent(&parts);
        tracing::info!(method = %method, uri, slug, client, "mcp request");
    }
    let response = match method.as_str() {
        "server/discover" => maybe_respond(id_present, id, discover_response),
        "initialize" => maybe_respond(id_present, id, |id| {
            initialize_response(id, &params, &state)
        }),
        "tools/list" => {
            let response = tools_list_response(id.clone(), &state).await;
            maybe_respond(id_present, id, |_| response)
        }
        "tools/call" => {
            let response = tools_call(id.clone(), params, principal, &state).await;
            maybe_respond(id_present, id, |_| response)
        }
        "resources/list" => maybe_respond(id_present, id, |id| resources_list(id, &state)),
        "resources/read" => {
            let response = resources_read(id.clone(), &params, principal, &state).await;
            maybe_respond(id_present, id, |_| response)
        }
        "updateModelContext" => {
            let response = update_model_context(id.clone(), &params, principal, &state).await;
            maybe_respond(id_present, id, |_| response)
        }
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
            // A notification Triton cannot accept: an HTTP error status,
            // the JSON-RPC error without an id. A request: -32601, and on
            // a 2026-07-28 request `404 Not Found` (Streamable HTTP,
            // Protocol Version Header).
            let status = if !id_present {
                StatusCode::BAD_REQUEST
            } else if modern {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            };
            return with_status(
                rpc_error(id, CODE_METHOD_NOT_FOUND, &err.to_string()),
                status,
            );
        }
    };
    if modern {
        modernize(response, &method).await
    } else {
        response
    }
}

fn user_agent(parts: &Parts) -> &str {
    parts
        .headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// If `id` was present, return the response. Otherwise (notification)
/// return `202 Accepted` with no body (Streamable HTTP, "Sending
/// Messages" rule 5).
fn maybe_respond<F>(id_present: bool, id: Value, build: F) -> Response
where
    F: FnOnce(Value) -> Response,
{
    if id_present {
        build(id)
    } else {
        StatusCode::ACCEPTED.into_response()
    }
}

fn with_status(mut response: Response, status: StatusCode) -> Response {
    *response.status_mut() = status;
    response
}

/// JSON-RPC error triple for a request rejected before dispatch.
type EraRejection = (i32, String, Option<Value>);

/// `Ok(true)` for a 2026-07-28 request, `Ok(false)` for a handshake-era
/// one, `Err` for a 2026-07-28-style request Triton cannot serve: an
/// unimplemented version (`UnsupportedProtocolVersionError`, -32022) or
/// headers that contradict the body (`HeaderMismatch`, -32020).
fn era(parts: &Parts, params: &Value, method: &str) -> Result<bool, EraRejection> {
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    let header_version = header("mcp-protocol-version");
    let meta_version = params
        .get("_meta")
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str);
    if let (Some(h), Some(m)) = (header_version, meta_version)
        && h != m
    {
        return Err((
            CODE_HEADER_MISMATCH,
            format!("Header mismatch: MCP-Protocol-Version header '{h}' does not match body '{m}'"),
            None,
        ));
    }
    let requested = meta_version.or(header_version.filter(|v| !SUPPORTED_MCP_VERSIONS.contains(v)));
    let Some(requested) = requested else {
        return Ok(false);
    };
    if !MODERN_MCP_VERSIONS.contains(&requested) {
        return Err((
            CODE_UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version".to_string(),
            Some(json!({ "supported": supported_versions(), "requested": requested })),
        ));
    }
    if let Some(h) = header("mcp-method")
        && h != method
    {
        return Err((
            CODE_HEADER_MISMATCH,
            format!("Header mismatch: Mcp-Method header '{h}' does not match body '{method}'"),
            None,
        ));
    }
    Ok(true)
}

/// Every version Triton serves, newest first: the stateless revisions,
/// then the handshake-era ones.
fn supported_versions() -> Vec<&'static str> {
    MODERN_MCP_VERSIONS
        .iter()
        .chain(SUPPORTED_MCP_VERSIONS.iter().rev())
        .copied()
        .collect()
}

/// `server/discover` (MCP 2026-07-28): supported versions, capabilities
/// and identity in one stateless result. The capabilities are those
/// `initialize` advertises. Cache scope is private: the endpoint is
/// authenticated and per customer.
fn discover_response(id: Value) -> Response {
    rpc_ok(
        id,
        json!({
            "resultType": "complete",
            "supportedVersions": supported_versions(),
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "triton",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            },
            "ttlMs": DISCOVER_TTL_MS,
            "cacheScope": "private"
        }),
    )
}

/// A 2026-07-28 result names its `resultType`, and a list or resource-read
/// result carries cache hints (`CacheableResult`). Handlers build the
/// handshake-era shape; this adds the missing fields to a successful
/// JSON-RPC result.
async fn modernize(response: Response, method: &str) -> Response {
    let (head, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return (head, axum::body::Body::empty()).into_response();
    };
    let Ok(mut doc) = serde_json::from_slice::<Value>(&bytes) else {
        return (head, axum::body::Body::from(bytes)).into_response();
    };
    if let Some(result) = doc.get_mut("result").and_then(Value::as_object_mut) {
        result
            .entry("resultType")
            .or_insert_with(|| json!("complete"));
        if matches!(method, "tools/list" | "resources/list" | "resources/read") {
            // Lists and resource reads are `CacheableResult`s: both hints
            // are required, and a host refuses a result without them.
            // Tool and resource lists follow the upstream registry, and a
            // proxied `ui://` read is the upstream's, so all are
            // immediately stale unless the upstream said otherwise.
            result.entry("ttlMs").or_insert_with(|| json!(0));
            result
                .entry("cacheScope")
                .or_insert_with(|| json!("private"));
        }
    }
    let mut head = head;
    head.headers.remove(axum::http::header::CONTENT_LENGTH);
    (head, axum::body::Body::from(doc.to_string())).into_response()
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
            let mut tool = json!({
                "name": d.name,
                "inputSchema": object_schema(d.input_schema),
                "x-triton": { "returns_a2ui": d.returns_a2ui, "upstream": d.upstream },
            });
            // The upstream's own description and annotations, verbatim;
            // absent (not null) when it gave none — both are optional in MCP.
            if let Some(description) = d.description {
                tool["description"] = Value::String(description);
            }
            if let Some(annotations) = d.annotations {
                tool["annotations"] = annotations;
            }
            // A tool that answers with a surface names the answer-card
            // widget, so an MCP Apps host (Claude, ChatGPT) renders it.
            if state.widget && d.upstream && d.returns_a2ui {
                tool["_meta"] = widget_tool_meta();
            }
            tool
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

    let outcome = match &state.tasks {
        Some(runner)
            if state.dispatcher.has_upstream() && !state.dispatcher.is_in_process(&tool_name) =>
        {
            let returns_a2ui = state.dispatcher.returns_a2ui(&tool_name);
            match run_as_task(runner, &tool_name, args, principal, returns_a2ui).await {
                Ok(McpOutcome::Done(d)) => Ok(d),
                Ok(McpOutcome::Filed(filed)) => return rpc_ok(id, filed),
                Ok(McpOutcome::AgentError(message)) => {
                    return rpc_error(id, CODE_TOOL_PROVIDER, &message);
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            state
                .dispatcher
                .invoke(&tool_name, args, principal, "mcp")
                .await
        }
    };
    match outcome {
        Ok(mut d) => {
            let trace_id = d.trace_id.clone();
            // Issue #143 (A): an MCP-Apps upstream (e.g. peacock's
            // `render_report`) returns its UI resource link under the
            // result's `_meta.ui.*`. Capture it before any A2UI wrap
            // rewrites `d.result`, so it can be surfaced on the
            // `tools/call` response `_meta` for the host to render.
            // Unknown `ui.*` siblings are preserved verbatim, but only a
            // structured object is reflected (#143 review, [MEDIUM]):
            // refuse to mirror an arbitrary scalar/array blob a hostile
            // upstream might stuff under `_meta.ui` to bloat the response.
            // (The cross-upstream `resourceUri` confused-deputy surface
            // noted in review is bounded by the trust model — every
            // upstream is operator-registered in TRITON_STATIC_UPSTREAMS
            // and any caller can already reach any registered tool — and
            // documented in realizations.md §7.)
            let ui_meta = d
                .result
                .get("_meta")
                .and_then(|m| m.get("ui"))
                .filter(|ui| ui.is_object())
                .cloned();
            // An upstream agent may attach a per-turn tool trace under
            // `_meta.tool_trace` (which tools ran + the data they touched) —
            // a host-render debug sidecar, mirrored to the response `_meta`
            // exactly like `ui` (and captured before the A2UI wrap rewrites
            // `d.result`). Only a structured array is reflected; a scalar/
            // object blob is ignored, so an upstream can't bloat the response.
            let tool_trace = d
                .result
                .get("_meta")
                .and_then(|m| m.get("tool_trace"))
                .filter(|t| t.is_array())
                .cloned();
            // Flatten the tool's A2UI surface to human-readable Markdown
            // BEFORE the A2UI wrap rewrites `d.result` — the same prose the
            // spec-A2A face serves (prose + `![chart](image_url)` + clickable
            // sources). An MCP host with a text bubble (Copilot Studio's MCP
            // tool, ChatGPT) then shows a real summary and renders the
            // convergence chart inline, instead of the raw envelope JSON its
            // model would otherwise re-summarise into a dead `ui://` stub.
            // Structured clients still get the full typed envelope in
            // `structuredContent` below, unchanged.
            let reply_text = crate::a2a_spec::reply_text(&d.result);
            // The answer-card widget's data, from the canonical surface (and
            // before the A2UI wrap). A result that brings its own MCP App
            // (`_meta.ui`) keeps it; there is no card to draw then.
            let widget = (state.widget && ui_meta.is_none())
                .then(|| mcp_app::widget_data(&d.result))
                .flatten();
            if let Err(e) = wrap_a2ui_if_requested(&mut d, requested) {
                return rpc_error(id, code_for(&e), &e.to_string());
            }
            // MCP's `tools/call` result carries `content` (text-shaped
            // for UI display) and `structuredContent` (typed value).
            // `content` is the flattened prose+chart+sources (above);
            // `structuredContent` is the canonical typed envelope, the
            // same dict REST/A2A get.
            let mut env_v = envelope(&d);
            if let Some(w) = widget {
                env_v["widget"] = w;
            }
            let mut meta = json!({ "trace_id": trace_id });
            if let Some(ui) = ui_meta {
                meta["ui"] = ui;
            }
            if let Some(trace) = tool_trace {
                meta["tool_trace"] = trace;
            }
            rpc_ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": reply_text }],
                    "structuredContent": env_v,
                    "isError": false,
                    "_meta": meta
                }),
            )
        }
        Err(e) => {
            // An agent-side fault reaches the client (and through its model,
            // a person) as the neutral notice; the cause — possibly the
            // upstream's own status line — stays in the log.
            if matches!(e, TritonError::Tool(_) | TritonError::Provider(_)) {
                tracing::warn!(tool = %tool_name, error = %e, "mcp tools/call failed");
                rpc_error(id, code_for(&e), e.user_notice())
            } else {
                rpc_error(id, code_for(&e), &e.to_string())
            }
        }
    }
}

enum McpOutcome {
    Done(triton_core::Dispatch),
    /// The upstream went asynchronous and did not finish within
    /// `TRITON_MCP_ASYNC_WAIT_SECS`: the complete `tools/call` result.
    Filed(Value),
    /// The upstream failed the call with its own `error` event; the
    /// message is written for the caller (contract §7).
    AgentError(String),
}

/// Run an upstream tool call as a task. Until the upstream says it
/// went asynchronous, this waits like a plain call; after that, at most
/// `mcp_wait`. A task that does not finish by then is answered "filed":
/// the upstream's receipt as text, and `{status: "filed", task_id,
/// check_after_seconds}` as structured content. The task keeps running;
/// the caller checks back through the upstream's own status tool.
async fn run_as_task(
    runner: &crate::tasks::Runner,
    tool: &str,
    args: Value,
    principal: triton_core::Principal,
    returns_a2ui: bool,
) -> Result<McpOutcome, TritonError> {
    use crate::tasks::{TaskEvent, TaskRecord, owner_key};
    let started = std::time::Instant::now();
    let trace_id = principal.trace_id.clone();
    let rec = TaskRecord::new(&trace_id, &trace_id, tool, Some(owner_key(&principal)));
    let mut rx = runner
        .start(
            rec,
            crate::tasks::Start::Dispatch(args),
            principal,
            "mcp",
            Default::default(),
            // F5: a plain call is no task; only an asynchronous one is
            // stored (so the client can be told it was filed).
            crate::tasks::Persist::OnAsync,
        )
        .map_err(|e| TritonError::Tool(e.to_string()))?;
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut check_after = runner.settings.mcp_check_after_secs;
    let mut receipt: Option<String> = None;
    let mut upstream_task_id: Option<String> = None;
    loop {
        let next = match deadline {
            Some(d) => match tokio::time::timeout_at(d, rx.recv()).await {
                Ok(r) => r,
                Err(_) => break,
            },
            None => rx.recv().await,
        };
        match next {
            Ok(TaskEvent::Async {
                check_after_seconds,
                receipt: r,
                upstream_task_id: up,
                ..
            }) => {
                upstream_task_id = Some(up);
                deadline.get_or_insert(tokio::time::Instant::now() + runner.settings.mcp_wait);
                if let Some(c) = check_after_seconds {
                    check_after = c;
                }
                receipt = r.or(receipt);
            }
            Ok(TaskEvent::Done(result)) => {
                return Ok(McpOutcome::Done(triton_core::Dispatch {
                    result,
                    trace_id,
                    latency_ms: started.elapsed().as_millis() as u64,
                    returns_a2ui,
                }));
            }
            Ok(TaskEvent::Failed { class, message }) => {
                if class == crate::tasks::CLASS_AGENT {
                    return Ok(McpOutcome::AgentError(message));
                }
                return Err(crate::tasks::error_of(&class, message));
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                // The runner ended; the store has its outcome.
                return match runner.store.get(&trace_id) {
                    Some(r) if r.state == crate::tasks::TaskState::Completed => {
                        Ok(McpOutcome::Done(triton_core::Dispatch {
                            result: r.result.unwrap_or_else(|| json!({ "surface": { "text": r.result_text.unwrap_or_default() } })),
                            trace_id,
                            latency_ms: started.elapsed().as_millis() as u64,
                            returns_a2ui,
                        }))
                    }
                    Some(r) if r.error_class.as_deref() == Some(crate::tasks::CLASS_AGENT) => {
                        Ok(McpOutcome::AgentError(r.error.unwrap_or_default()))
                    }
                    Some(r) => Err(crate::tasks::stored_error(&r)),
                    None => Err(TritonError::Tool("the call failed".into())),
                };
            }
        }
    }
    let text = receipt.clone().unwrap_or_else(|| {
        format!(
            "The request is filed and still running. Check again in about {check_after} seconds."
        )
    });
    // F7: the id the agent's own status tool knows; Triton's trace id
    // stays in `_meta`.
    let mut structured = json!({
        "status": "filed",
        "task_id": upstream_task_id.unwrap_or_else(|| trace_id.clone()),
        "check_after_seconds": check_after,
    });
    if let Some(r) = receipt {
        structured["receipt"] = json!(r);
    }
    Ok(McpOutcome::Filed(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
        "_meta": { "trace_id": trace_id },
    })))
}

/// Minimal `resources/list` (#143 review, [NIT]): `initialize` advertises
/// a `resources` capability, so the method must exist rather than 404.
/// Only the local runtime stub is enumerable; upstream-owned `ui://`
/// resources are loaded on demand via `resources/read` (#143 B), not
/// listed here (upstream enumeration is optional in v1).
fn resources_list(id: Value, state: &McpState) -> Response {
    let mut resources = vec![json!({
        "uri": "ui://triton/runtime.html",
        "name": "triton-runtime",
        "mimeType": "text/html"
    })];
    if state.widget {
        resources.push(json!({
            "uri": mcp_app::resource_uri(),
            "name": "answer-card",
            "title": "Answer card",
            "description": WIDGET_DESCRIPTION,
            "mimeType": mcp_app::MIME_TYPE,
            "_meta": widget_resource_meta(),
        }));
    }
    rpc_ok(id, json!({ "resources": resources }))
}

const WIDGET_DESCRIPTION: &str = "Shows the answer as a card: a table, facts, a map or a form, \
     with buttons that send a complete follow-up request or open a link.";

/// MCP requires a tool's `inputSchema` to be an object schema
/// (`"type": "object"`); strict clients (the MCP Apps reference host's SDK)
/// refuse the whole tool list otherwise. An upstream that published none,
/// or one without `type`, gets it filled in.
fn object_schema(schema: Value) -> Value {
    match schema {
        Value::Object(mut m) => {
            m.entry("type").or_insert_with(|| json!("object"));
            Value::Object(m)
        }
        _ => json!({ "type": "object" }),
    }
}

/// The tool descriptor `_meta` that points an MCP Apps host at the widget:
/// the MCP Apps key (`ui.resourceUri`) and the ChatGPT Apps SDK alias
/// (`openai/outputTemplate`). The flat `ui/resourceUri` is deprecated.
fn widget_tool_meta() -> Value {
    let uri = mcp_app::resource_uri();
    json!({
        "ui": { "resourceUri": uri },
        "openai/outputTemplate": uri,
    })
}

/// The resource `_meta`: no network at all (inline CSS and JS). The card
/// draws its own border, so an answer without a card leaves no empty frame.
fn widget_resource_meta() -> Value {
    json!({
        "ui": {
            "csp": { "connectDomains": [], "resourceDomains": [] },
            "prefersBorder": false,
        },
        // ChatGPT's own spelling of the same (empty) policy.
        "openai/widgetCSP": { "connect_domains": [], "resource_domains": [] },
        "openai/widgetDescription": WIDGET_DESCRIPTION,
        "openai/widgetPrefersBorder": false,
    })
}

async fn resources_read(
    id: Value,
    params: &Value,
    principal: triton_core::Principal,
    state: &McpState,
) -> Response {
    // #143 review ([LOW]): reject a missing/non-string `uri` instead of
    // defaulting to "" and surfacing a vaguer downstream error.
    let Some(uri) = params["uri"].as_str() else {
        return rpc_error(id, CODE_INVALID_PARAMS, "params.uri MUST be a string");
    };
    let uri = uri.to_string();
    // The `ui://triton/...` runtime resource is served locally — no
    // upstream owns it (FR-A-6 stub).
    if uri == "ui://triton/runtime.html" {
        return rpc_ok(
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
        );
    }
    // The answer-card widget, any revision of its URI (a host may have
    // listed the tools before an upgrade): always the current page.
    if state.widget && mcp_app::is_widget_uri(&uri) {
        return rpc_ok(
            id,
            json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": mcp_app::MIME_TYPE,
                    "text": mcp_app::widget_html(),
                    "_meta": widget_resource_meta(),
                }]
            }),
        );
    }
    // Issue #143 (B): any other `ui://` URI is proxied to its owning
    // upstream (resolved by authority through the dispatch registry),
    // reusing the same bearer-mint + SSRF + breaker + audit as a
    // `tools/call`. An unknown owner surfaces as a JSON-RPC error.
    match state.dispatcher.read_resource(&uri, principal).await {
        Ok(d) => rpc_ok(id, d.result),
        Err(e) => rpc_error(id, code_for(&e), &e.to_string()),
    }
}

/// Issue #143 (C): relay an in-iframe `updateModelContext` record to the
/// owning upstream. Params: `{ uri, record }` — `uri` is the `ui://`
/// resource the iframe was loaded from (its authority routes the relay);
/// `record` is the compact `{report_id, params, salient_summary}` payload
/// forwarded **unmodified**. The host owns the model-context channel; the
/// upstream's ack is relayed back.
async fn update_model_context(
    id: Value,
    params: &Value,
    principal: triton_core::Principal,
    state: &McpState,
) -> Response {
    let Some(uri) = params["uri"].as_str() else {
        return rpc_error(id, CODE_INVALID_PARAMS, "params.uri MUST be a string");
    };
    let uri = uri.to_string();
    // The record rides through untouched — Triton MUST NOT inspect or
    // expand it (#143 C). #143 review ([LOW]): require the `record`
    // member to be present (its value is still forwarded verbatim,
    // including an explicit `null`), rather than silently relaying a
    // synthesised `null` when the caller omitted it.
    let Some(record) = params.get("record").cloned() else {
        return rpc_error(id, CODE_INVALID_PARAMS, "params.record MUST be present");
    };
    match state
        .dispatcher
        .update_model_context(&uri, record, principal)
        .await
    {
        Ok(d) => rpc_ok(id, d.result),
        Err(e) => rpc_error(id, code_for(&e), &e.to_string()),
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
    rpc_error_with_data(id, code, message, None)
}

fn rpc_error_with_data(id: Value, code: i32, message: &str, data: Option<Value>) -> Response {
    (
        StatusCode::OK,
        Json(RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.to_string(),
                data,
            }),
        }),
    )
        .into_response()
}
