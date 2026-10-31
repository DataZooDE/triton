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

/// JSON-RPC error codes. The first four are standard JSON-RPC 2.0; the
/// `-32001` code is A2A's own `TaskNotFound`.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

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
    /// The card's skills (A2A `AgentSkill` objects). `None` derives them
    /// (see `a2a_v1::skills`).
    pub skills: Option<Vec<Value>>,
}

impl SpecA2aConfig {
    /// The endpoint a card advertises: the public URL plus the A2A path
    /// (the binary sets the path empty and the full endpoint as the URL).
    pub(crate) fn endpoint_url(&self) -> String {
        let base = self.public_url.trim_end_matches('/');
        let path = self.a2a_path.trim_matches('/');
        if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        }
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
        r = r.route(&format!("/.well-known/{name}"), card_route(name));
    }
    r.with_state(state)
}

/// `agent-card.json` is the 1.0 card (`?version=0.3` selects 0.3);
/// every other spelling is the 0.3 card Copilot Studio and Gemini
/// Enterprise read.
fn card_route(name: &str) -> axum::routing::MethodRouter<CardState> {
    let default = if *name == *"agent-card.json" {
        crate::a2a_v1::Version::V10
    } else {
        crate::a2a_v1::Version::V03
    };
    get(
        move |State(state): State<CardState>, uri: axum::http::Uri| async move {
            crate::a2a_v1::card_response(&state, uri.query(), default)
        },
    )
}

/// The whole spec face for one endpoint, to be merged into the router
/// nested at that endpoint: JSON-RPC (0.3 and 1.0) at its root, the 1.0
/// HTTP+JSON binding under `/v1`, and the endpoint-relative cards. Used by
/// the binary and the embedded host alike.
pub fn spec_routes(state: A2aState, config: SpecA2aConfig, card: CardState) -> Router {
    let config = Arc::new(config);
    jsonrpc_router(state.clone(), config.clone())
        .merge(crate::a2a_v1::rest_router(state, config))
        .merge(card_router_nested(card))
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
        .layer(axum::middleware::from_fn(crate::identity::bearer_challenge))
}

/// Why a per-agent slug was refused. See [`validate_agent_slug`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAgentSlug(pub String);

impl std::fmt::Display for InvalidAgentSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidAgentSlug {}

/// Slugs that would shadow, or be confused with, routes the A2A nest
/// already serves (`/a2a/message:send`, `/a2a/.well-known/*`). The
/// character rule below already excludes `:` and `.`; these are the
/// plain-word names reserved for the base surface.
const RESERVED_SLUGS: &[&str] = &["message", "tasks", "well-known"];

/// A per-agent slug must be `[a-z0-9-]+`, must not start or end with `-`,
/// and must not be a reserved name. The character rule is what keeps a
/// per-agent route from ever colliding with a Triton route under the same
/// `/a2a` nest: every existing one contains `:` (`message:send`) or `.`
/// (`.well-known`), and a slug can contain neither.
pub fn validate_agent_slug(slug: &str) -> Result<(), InvalidAgentSlug> {
    if slug.is_empty() || slug.len() > 64 {
        return Err(InvalidAgentSlug(format!(
            "a2a agent slug `{slug}` must be 1-64 characters"
        )));
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(InvalidAgentSlug(format!(
            "a2a agent slug `{slug}` may contain only [a-z0-9-]"
        )));
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err(InvalidAgentSlug(format!(
            "a2a agent slug `{slug}` must not start or end with `-`"
        )));
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(InvalidAgentSlug(format!(
            "a2a agent slug `{slug}` is reserved"
        )));
    }
    Ok(())
}

/// One agent's own spec-A2A face, to be merged into the router nested
/// under `/a2a`: JSON-RPC at `/<slug>` (→ `POST /a2a/<slug>`) dispatching
/// prose to that agent's `default_tool`, and its Agent Card at every
/// [`CARD_FILENAMES`] spelling under `/<slug>/.well-known/` — the
/// endpoint-relative location Copilot Studio's runtime resolves, and the
/// only one a per-agent card can have (the origin root belongs to the
/// primary card).
///
/// The JSON-RPC route shares `state` — the SAME `IdentityProvider` and
/// task store as the root `/a2a` route — so auth and `tasks/get` scoping
/// are identical. Routes are static (one per configured slug), so an
/// unconfigured slug is a plain 404.
pub fn agent_router(
    slug: &str,
    state: A2aState,
    card: CardState,
) -> Result<Router, InvalidAgentSlug> {
    validate_agent_slug(slug)?;
    let mut cards = Router::new();
    for name in CARD_FILENAMES {
        cards = cards.route(&format!("/{slug}/.well-known/{name}"), card_route(name));
    }
    let config = card.config.clone();
    Ok(Router::new()
        .route(&format!("/{slug}"), post(jsonrpc))
        .with_state(SpecState {
            a2a: state.clone(),
            config: config.clone(),
        })
        .nest(
            &format!("/{slug}"),
            crate::a2a_v1::rest_router(state, config),
        )
        .layer(axum::middleware::from_fn(crate::identity::bearer_challenge))
        .merge(cards.with_state(card)))
}

#[derive(Clone)]
pub(crate) struct SpecState {
    pub(crate) a2a: A2aState,
    pub(crate) config: Arc<SpecA2aConfig>,
}

// The Agent Card is built in `a2a_v1` (`card_v10`, `card_v03`). It is
// **unauthenticated on purpose**: discovery is what a caller does BEFORE it
// has a credential, and the card's whole job is to say which credential to
// bring. It contains only what is already public — the agent's name, its
// endpoint, its skills, and the issuer/audience pairs it accepts.

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
            // needs the HTTP status its client library already handles. A
            // token without the delegated scope is a 403 (ADR-22).
            let status = match e {
                triton_core::TritonError::Forbidden(_) => StatusCode::FORBIDDEN,
                _ => StatusCode::UNAUTHORIZED,
            };
            let response = (
                status,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": INVALID_REQUEST, "message": e.to_string() },
                })),
            )
                .into_response();
            return state.a2a.identity.tag_rejection(&e, response);
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
        .or_else(|| parts.headers.get("a2a-extensions"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    tracing::info!(x_a2a_extensions = ?inbound_ext, "a2a inbound extension activation");
    let echo_a2ui = activates_a2ui(inbound_ext.as_deref());

    let req: RpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return rpc_error(&Value::Null, PARSE_ERROR, format!("invalid JSON: {e}")),
    };
    if req.jsonrpc.as_deref() != Some("2.0") {
        return rpc_error(&req.id, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }

    // The protocol version is chosen per request. An unsupported
    // version is answered in the form of the version the method name
    // belongs to (`message/send` is 0.3, `SendMessage` is 1.0).
    use crate::a2a_v1::{self as v1, A2aError, Version};
    let version = match v1::requested_version(&parts.headers, parts.uri.query()) {
        Ok(v) => v,
        Err(raw) => {
            let e = A2aError::VersionNotSupported(raw);
            return if req.method.contains('/') || !v1::is_v10_method(&req.method) {
                v1::rpc_error_v03(&req.id, &e)
            } else {
                v1::rpc_error_v10(&req.id, &e)
            };
        }
    };
    if version == Version::V10 {
        let method = req.method.clone();
        let mut resp =
            v1::jsonrpc_v10(state, principal, req.id, &method, req.params, echo_a2ui).await;
        if echo_a2ui
            && let Ok(v) = axum::http::HeaderValue::from_str(triton_core::a2ui::ge::EXTENSION_URI)
        {
            resp.headers_mut().insert("a2a-extensions", v);
        }
        return resp;
    }

    let mut resp = match req.method.as_str() {
        "message/send" => message_send(state, principal, req, echo_a2ui).await,
        "message/stream" => message_stream(state, principal, req, echo_a2ui).await,
        "tasks/get" => tasks_get(state, req, &principal),
        "tasks/cancel" => {
            let e = match req.params.get("id").and_then(Value::as_str) {
                None => A2aError::InvalidParams("params.id is required".into()),
                Some(id) => match v1::readable(&state.a2a, &principal, id).map(|(r, _)| r) {
                    Ok(_) => A2aError::TaskNotCancelable,
                    Err(e) => e,
                },
            };
            v1::rpc_error_v03(&req.id, &e)
        }
        "tasks/resubscribe" => {
            let framing = v1::Framing::JsonRpc03(req.id.clone());
            match v1::op_subscribe(&state, &principal, &req.params, framing, false) {
                Ok(r) => r,
                // The official 0.3 clients read a streaming method's answer
                // as an event stream only, errors included.
                Err(e) => v1::error_stream(json!({
                    "jsonrpc": "2.0", "id": req.id, "error": e.body_v03(),
                })),
            }
        }
        "tasks/pushNotificationConfig/set"
        | "tasks/pushNotificationConfig/get"
        | "tasks/pushNotificationConfig/list"
        | "tasks/pushNotificationConfig/delete" => {
            v1::rpc_error_v03(&req.id, &A2aError::PushNotSupported)
        }
        "agent/getAuthenticatedExtendedCard" => {
            v1::rpc_error_v03(&req.id, &A2aError::ExtendedCardNotConfigured)
        }
        // Named explicitly so a caller learns which methods exist rather
        // than only that this one does not.
        other => rpc_error(
            &req.id,
            METHOD_NOT_FOUND,
            format!(
                "unsupported method `{other}`; this agent implements message/send, message/stream, \
                 tasks/get, tasks/cancel, tasks/resubscribe (A2A 0.3; send A2A-Version: 1.0 for 1.0)"
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

/// Did the client activate the A2UI extension? `X-A2A-Extensions` is a
/// comma-separated list of extension URIs; one must equal the A2UI URI
/// exactly. A substring match would let `…/v0.9-not-really` (or any URI that
/// merely embeds it) switch the turn to native components.
fn activates_a2ui(header: Option<&str>) -> bool {
    header.is_some_and(|h| {
        h.split(',')
            .any(|uri| uri.trim() == triton_core::a2ui::ge::EXTENSION_URI)
    })
}

/// [`activates_a2ui`] over a request's headers: 0.3's `X-A2A-Extensions`
/// or 1.0's `A2A-Extensions`.
pub(crate) fn activates_a2ui_headers(headers: &axum::http::HeaderMap) -> bool {
    ["x-a2a-extensions", "a2a-extensions"]
        .iter()
        .any(|name| activates_a2ui(headers.get(*name).and_then(|v| v.to_str().ok())))
}

/// [`turn_context`] for a spec state.
pub(crate) fn turn_context_for(
    state: &SpecState,
    context_id: &str,
    a2ui: bool,
) -> triton_core::ContextOverrides {
    turn_context(
        &state.config,
        state.a2a.dispatcher.surfaces(),
        context_id,
        a2ui,
    )
}

/// The per-turn context spec A2A adds to a dispatch (#352): `_triton`
/// metadata (`protocol`, `a2a_context_id`, and `agent` for a `/a2a/<slug>`
/// face) and the surface, which depends on whether *this* request activated
/// A2UI. Without A2UI the channel's registered `a2a` surface applies.
fn turn_context(
    config: &SpecA2aConfig,
    surfaces: &triton_core::SurfaceRegistry,
    context_id: &str,
    a2ui: bool,
) -> triton_core::ContextOverrides {
    let mut meta = serde_json::Map::new();
    meta.insert("protocol".into(), json!("a2a"));
    meta.insert("a2a_context_id".into(), json!(context_id));
    if let Some(slug) = config.a2a_path.strip_prefix("/a2a/") {
        meta.insert("agent".into(), json!(slug));
    }
    triton_core::ContextOverrides {
        triton_meta: meta,
        // The registry's A2UI variant carries the operator's guidance; a host
        // that replaced the registry without one still gets the profile.
        surface: a2ui.then(|| {
            surfaces
                .get(triton_core::surface::A2A_A2UI_KEY)
                .unwrap_or_else(|| {
                    std::sync::Arc::new(triton_core::SurfaceDescriptor::from_profile(
                        &triton_core::surface::A2A_A2UI,
                        None,
                    ))
                })
        }),
    }
}

/// Marks a stream error whose message is already the text a person sees
/// ([`crate::tasks::user_text`]) — never audited, local to this module.
const USER_TEXT: &str = "a2a_user_text";

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
fn text_from_params(params: &Value) -> Option<Inbound> {
    let parts = params.get("message")?.get("parts")?.as_array()?;
    // A2UI button click: Gemini Enterprise posts back the action as a
    // `application/json+a2ui` DataPart. Its signed action id names the
    // complete message to send (or a document to open) — this is what makes
    // a follow-up button re-ask instead of the agent seeing GE's "User
    // action triggered." placeholder text. Fall back to the text parts.
    if let Some(action) = a2ui_action(parts) {
        return Some(match action {
            triton_core::a2ui::ge::GeAction::Message(text) => Inbound {
                text,
                open_doc: None,
                missing: None,
            },
            // A form submit without its required inputs sends nothing.
            triton_core::a2ui::ge::GeAction::Incomplete(names) => Inbound {
                text: String::new(),
                open_doc: None,
                missing: Some(format!("Choose {} first.", names.join(", "))),
            },
            triton_core::a2ui::ge::GeAction::OpenDoc {
                label,
                skill,
                id,
                url,
            } => Inbound {
                text: label.clone(),
                open_doc: Some(OpenDoc {
                    label,
                    skill,
                    id,
                    url,
                }),
                missing: None,
            },
        });
    }
    let text = parts
        .iter()
        .filter(|p| p.get("kind").and_then(Value::as_str).unwrap_or("text") == "text")
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Inbound {
        text: trimmed.to_string(),
        open_doc: None,
        missing: None,
    })
}

/// One inbound spec-A2A turn: the text to dispatch, or — for a source
/// button — the document to open instead of an agent turn.
struct Inbound {
    text: String,
    open_doc: Option<OpenDoc>,
    /// A form submit that lacked required inputs: refused with this text.
    missing: Option<String>,
}

struct OpenDoc {
    label: String,
    skill: String,
    id: String,
    url: String,
}

impl OpenDoc {
    fn as_ref(&self) -> triton_core::a2ui::ge::OpenDocRef<'_> {
        triton_core::a2ui::ge::OpenDocRef {
            label: &self.label,
            skill: &self.skill,
            id: &self.id,
            url: &self.url,
        }
    }
}

/// The action of an inbound A2UI `action` DataPart. The client-to-server
/// A2UI message rides `DataPart.data` (an object or a one-element array)
/// as `{version, action:{name, context, sourceComponentId}}`. A resolved
/// `context.question` wins when a host forwards one (GE does not — it posts
/// `context:{}`); otherwise the signed, self-contained
/// `sourceComponentId` Triton minted for the button names the action
/// (`ge::action_for`). Absent or forged ⇒ `None` (a normal text turn).
///
/// Both Part shapes count as a data part: 0.3's `{kind: "data", data}` and
/// 1.0's `{data, mediaType?}` (a 1.0 Part has no `kind`; its content is a
/// oneof, so a `data` member is the data part).
pub(crate) fn a2ui_action(parts: &[Value]) -> Option<triton_core::a2ui::ge::GeAction> {
    for p in parts {
        let is_data = match p.get("kind").and_then(Value::as_str) {
            Some(kind) => kind == "data",
            None => p.get("data").is_some(),
        };
        if !is_data {
            continue;
        }
        let Some(data) = p.get("data") else {
            continue;
        };
        // `data` may be a single message object or an array of messages.
        let msgs: Vec<&Value> = match data {
            Value::Array(a) => a.iter().collect(),
            obj => vec![obj],
        };
        for m in msgs {
            let Some(action) = m.get("action") else {
                continue;
            };
            // A signed component id names the action; its form inputs come
            // from the context. Only an action Triton did not sign may
            // carry its message as `context.question` (crew F6).
            let context = action.get("context").cloned().unwrap_or(Value::Null);
            if let Some(cid) = action.get("sourceComponentId").and_then(Value::as_str)
                && let Some(a) = triton_core::a2ui::ge::action_for_with(cid, &context)
            {
                return Some(a);
            }
            if let Some(q) = context
                .get("question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(triton_core::a2ui::ge::GeAction::Message(q.to_string()));
            }
        }
    }
    None
}

async fn message_send(
    state: SpecState,
    principal: triton_core::Principal,
    req: RpcRequest,
    a2ui: bool,
) -> Response {
    let Some(inbound) = text_from_params(&req.params) else {
        return rpc_error(
            &req.id,
            INVALID_PARAMS,
            "params.message.parts must contain at least one non-empty text part",
        );
    };
    // A form submit without a required choice: nothing is sent; the reply
    // says which choice is missing, as an ordinary agent message.
    if let Some(missing) = &inbound.missing {
        return rpc_ok(
            &req.id,
            json!({
                "kind": "message", "role": "agent",
                "messageId": uuid::Uuid::new_v4().to_string(),
                "parts": [ { "kind": "text", "text": missing } ],
            }),
        );
    }
    let text = inbound.text.clone();
    // A2A 0.3.0 requires `contextId` on Task; honor a client-supplied
    // `message.contextId`, else generate one (strict clients reject its
    // absence). Also stamped on the returned Message for follow-ups.
    let context_id = req
        .params
        .get("message")
        .and_then(|m| m.get("contextId"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // #352: one context for the whole turn, re-dispatches included.
    let overrides = turn_context(
        &state.config,
        state.a2a.dispatcher.surfaces(),
        &context_id,
        a2ui,
    );

    // A GE source button click resolves to an open-doc sentinel (not a real
    // agent turn): reply directly with the document surface — native Material
    // by default (a render_report(document) dispatch), or the /docs iframe
    // when TRITON_GE_DOC_IFRAME.
    if let Some(doc) = inbound.open_doc.as_ref().map(OpenDoc::as_ref) {
        let parts = doc_open_parts(&state.a2a.dispatcher, &principal, &doc, &overrides).await;
        let msg = json!({
            "kind": "message",
            "role": "agent",
            "messageId": uuid::Uuid::new_v4().to_string(),
            "parts": parts,
            "contextId": context_id,
        });
        return rpc_ok(&req.id, msg);
    }

    // #635 P6 — disconnect-safe by construction, and long-running:
    // the dispatch runs in a SPAWNED task runner that records every state
    // change (the clamped reply on completion) in the task store. The
    // handler merely waits on the runner's events, so a client hanging up
    // mid-turn drops the wait, never the dispatch — the answer completes
    // and `tasks/get` returns it. This is the same bug class as the Teams
    // webhook 499 (a 23s answer computed and lost to a cancelled future).
    let vega_dispatcher = state.a2a.dispatcher.clone();
    let vega_principal = principal.clone();
    let vega_overrides = overrides.clone();
    let (rec, mut rx) =
        match crate::a2a_v1::start_task(&state, &principal, &text, &context_id, overrides) {
            Ok(x) => x,
            Err(e) => return rpc_error(&req.id, INTERNAL_ERROR, e.message()),
        };

    if wants_immediate_task(&req.params) {
        // The spec: return the Task right away; the client polls
        // `tasks/get`. The spawned dispatch keeps running.
        let task = json!({
            "kind": "task",
            "id": rec.id,
            "contextId": context_id,
            "status": { "state": "working" },
        });
        return rpc_ok(&req.id, task);
    }

    // Blocking is the default, capped (`TRITON_A2A_BLOCK_SECS`) so a
    // platform's HTTP timeout is not reached: at the cap the working task
    // comes back and the caller polls or resubscribes.
    match crate::tasks::wait_terminal(&mut rx, state.a2a.task_settings.block).await {
        Some(crate::tasks::TaskEvent::Done(mut result)) => {
            // Expand a report into a native interactive VegaChart for GE.
            inject_report_vega(
                &vega_dispatcher,
                &mut result,
                &vega_principal,
                &vega_overrides,
            )
            .await;
            let msgs = triton_core::a2ui::ge::build_messages(&result);
            let reply = text_part(&result, msgs.is_some());
            let mut parts = vec![json!({ "kind": "text", "text": reply })];
            if let Some(msgs) = msgs {
                parts.extend(triton_core::a2ui::ge::data_parts(msgs));
            }
            let msg = json!({
                "kind": "message",
                "role": "agent",
                "messageId": uuid::Uuid::new_v4().to_string(),
                "parts": parts,
                // The task id, so a caller can follow up via tasks/get.
                "taskId": rec.id,
                "contextId": context_id,
            });
            rpc_ok(&req.id, msg)
        }
        Some(crate::tasks::TaskEvent::Failed { class, message }) => {
            tracing::warn!(task = %rec.id, class = %class, error = %message, "a2a message/send failed");
            rpc_error(
                &req.id,
                INTERNAL_ERROR,
                crate::tasks::user_text(&class, &message),
            )
        }
        _ => {
            let rec = state.a2a.tasks.get(&rec.id).unwrap_or(rec);
            rpc_ok(
                &req.id,
                crate::a2a_v1::task_v03(&crate::a2a_v1::handover(rec)),
            )
        }
    }
}

/// Spec `message/stream` (#635 P6): SSE of JSON-RPC responses — the
/// initial Task (`working`), a `TaskArtifactUpdateEvent` per token
/// delta (append: true; from the in-process streaming seam when the
/// tool opts in, else nothing until the end), a completed prose artifact,
/// an optional separate A2UI artifact, and a terminal `TaskStatusUpdateEvent`
/// (`final: true`, closing the stream per spec). Task state is
/// recorded exactly as message/send records it, so `tasks/get` works
/// on streamed turns too.
async fn message_stream(
    state: SpecState,
    principal: triton_core::Principal,
    req: RpcRequest,
    a2ui: bool,
) -> Response {
    use futures::StreamExt as _;

    let Some(inbound) = text_from_params(&req.params) else {
        return rpc_error(
            &req.id,
            INVALID_PARAMS,
            "params.message.parts must contain at least one non-empty text part",
        );
    };
    if let Some(missing) = &inbound.missing {
        // As in message/send: one agent message, the stream ends.
        let frame = json!({ "jsonrpc": "2.0", "id": req.id, "result": {
            "kind": "message", "role": "agent",
            "messageId": uuid::Uuid::new_v4().to_string(),
            "parts": [ { "kind": "text", "text": missing } ],
        } });
        let sse = futures::stream::iter([Ok::<_, std::convert::Infallible>(
            axum::response::sse::Event::default().data(frame.to_string()),
        )]);
        return axum::response::Sse::new(sse).into_response();
    }
    let text = inbound.text.clone();
    let trace_id = principal.trace_id.clone();
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
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // #352: one context for the whole turn, re-dispatches included.
    let overrides = turn_context(
        &state.config,
        state.a2a.dispatcher.surfaces(),
        &context_id,
        a2ui,
    );

    // A GE source button click (open-doc sentinel): emit a one-shot SSE — a
    // working task, the document surface as a final artifact, and a completed
    // status. Native Material by default (a render_report(document) dispatch),
    // or the /docs iframe when TRITON_GE_DOC_IFRAME.
    if let Some(doc) = inbound.open_doc.as_ref().map(OpenDoc::as_ref) {
        let parts = doc_open_parts(&state.a2a.dispatcher, &principal, &doc, &overrides).await;
        state.a2a.tasks.record_entry(
            &trace_id,
            TaskState::Completed,
            Some("opened source document"),
            None,
        );
        let rpc_id = req.id.clone();
        let rpc = move |result: Value| serde_json::json!({ "jsonrpc": "2.0", "id": rpc_id.clone(), "result": result });
        let artifact_id = uuid::Uuid::new_v4().to_string();
        let frames = vec![
            rpc(json!({
                "kind": "task", "id": trace_id, "contextId": context_id,
                "status": { "state": "working" },
            })),
            rpc(json!({
                "kind": "artifact-update", "taskId": trace_id, "contextId": context_id,
                "lastChunk": true,
                "artifact": { "artifactId": artifact_id, "parts": parts },
            })),
            rpc(json!({
                "kind": "status-update", "taskId": trace_id, "contextId": context_id,
                "status": { "state": "completed" }, "final": true,
            })),
        ];
        let sse = futures::stream::iter(frames).map(|v| {
            Ok::<axum::response::sse::Event, std::convert::Infallible>(
                axum::response::sse::Event::default().data(v.to_string()),
            )
        });
        return axum::response::Sse::new(sse)
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
            )
            .into_response();
    }

    // Kept for the terminal report→VegaChart expansion.
    let vega_dispatcher = state.a2a.dispatcher.clone();
    let vega_principal = principal.clone();
    let vega_overrides = overrides.clone();
    // The turn runs as a task; the stream follows its events until
    // it ends or `TRITON_A2A_STREAM_MAX_SECS` passes (then the client
    // resubscribes with `tasks/resubscribe`).
    let (_rec, rx) =
        match crate::a2a_v1::start_task(&state, &principal, &text, &context_id, overrides) {
            Ok(x) => x,
            Err(e) => return rpc_error(&req.id, INTERNAL_ERROR, e.message()),
        };
    let deadline = tokio::time::Instant::now() + state.a2a.task_settings.stream_max;
    // F4: a reader that falls behind the broadcast loses token deltas; the
    // final artifact then carries the whole reply (append: false).
    let lagged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lagged_rx = lagged.clone();
    let events = futures::stream::unfold((rx, false), move |(mut rx, ended)| {
        let lagged_rx = lagged_rx.clone();
        async move {
            use crate::tasks::TaskEvent as T;
            use triton_core::stream::StreamEvent as S;
            if ended {
                return None;
            }
            loop {
                let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) | Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        return None;
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        lagged_rx.store(true, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    Ok(Ok(ev)) => ev,
                };
                let terminal = ev.is_terminal();
                let out = match ev {
                    T::Token(t) => S::Token(t),
                    T::Progress(p) => S::Progress(json!({ "text": p.text, "step": p.step })),
                    T::Async { .. } => continue,
                    T::Done(v) => S::Done(v),
                    // The rendered text rides the error; the frame below
                    // shows it as it is (`USER_TEXT`).
                    T::Failed { class, message } => S::error_with_detail(
                        triton_core::TritonError::Tool(crate::tasks::user_text(&class, &message)),
                        USER_TEXT,
                    ),
                };
                return Some((out, (rx, terminal)));
            }
        }
    });

    let task_id = trace_id.clone();
    let rpc = move |result: Value| serde_json::json!({ "jsonrpc": "2.0", "id": rpc_id.clone(), "result": result });
    let initial = rpc(json!({
        "kind": "task",
        "id": task_id,
        "contextId": context_id,
        "status": { "state": "working" },
    }));

    let artifact_id = uuid::Uuid::new_v4().to_string();
    let a2ui_artifact_id = uuid::Uuid::new_v4().to_string();
    let task_for_frames = trace_id.clone();
    let ctx_for_frames = context_id.clone();
    let rpc_frames = rpc.clone();
    // Whether we streamed the answer as token deltas. If we did, the final
    // artifact must NOT resend the full reply text — the client accumulates
    // append-chunks, so a second full copy shows the answer twice (and drags
    // the trailing `![chart]` markdown in as literal text on hosts that render
    // A2UI, like Gemini Enterprise). When nothing streamed (a buffered tool),
    // the final artifact carries the text once.
    // `streamed_text` is shared across per-event futures (async closures can't
    // hold a `&mut` across the stream), so track it atomically.
    let streamed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let frames = events
        .then(move |ev| {
            use std::sync::atomic::Ordering;
            let rpc_frames = rpc_frames.clone();
            let task_for_frames = task_for_frames.clone();
            let ctx_for_frames = ctx_for_frames.clone();
            let artifact_id = artifact_id.clone();
            let a2ui_artifact_id = a2ui_artifact_id.clone();
            let streamed = streamed.clone();
            let lagged = lagged.clone();
            let vega_dispatcher = vega_dispatcher.clone();
            let vega_principal = vega_principal.clone();
            let vega_overrides = vega_overrides.clone();
            async move {
                let out: Vec<Value> = match ev {
                    triton_core::stream::StreamEvent::Token(t) => {
                        streamed.store(true, Ordering::Relaxed);
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
                    triton_core::stream::StreamEvent::Progress(p) => {
                        // A real step, as a non-final working status
                        // (mock 1j: status lines, replaced by the answer).
                        let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                        let mut update = json!({
                            "kind": "status-update",
                            "taskId": task_for_frames,
                            "contextId": ctx_for_frames,
                            "status": { "state": "working", "message": {
                                "kind": "message", "role": "agent",
                                "messageId": uuid::Uuid::new_v4().to_string(),
                                "parts": [{ "kind": "text", "text": text }],
                            } },
                            "final": false,
                        });
                        if let Some(step) = p.get("step").and_then(Value::as_str) {
                            update["metadata"] = json!({ "step": step });
                        }
                        vec![rpc_frames(update)]
                    }
                    triton_core::stream::StreamEvent::Tool(_)
                    | triton_core::stream::StreamEvent::Task(_) => Vec::new(),
                    triton_core::stream::StreamEvent::Done(v) => {
                        let mut v = v;
                        // Expand a report into a native interactive VegaChart
                        // (GE); best-effort, falls back to the static image.
                        inject_report_vega(
                            &vega_dispatcher,
                            &mut v,
                            &vega_principal,
                            &vega_overrides,
                        )
                        .await;
                        // The runner already recorded the task; only the frames
                        // remain. A lagged reader missed deltas: resend whole.
                        let did_stream =
                            streamed.load(Ordering::Relaxed) && !lagged.load(Ordering::Relaxed);
                        let a2ui_msgs = triton_core::a2ui::ge::build_messages(&v);
                        let reply = text_part(&v, a2ui_msgs.is_some());
                        // Keep the prose and A2UI as separate artifacts. Token
                        // deltas append to the prose artifact; putting A2UI in
                        // its last chunk makes clients merge structured parts
                        // into an already-rendered text artifact.
                        let mut frames = Vec::new();
                        // Streamed prose loses the clickable sources line (only in
                        // the final `reply`, which we skip); emit it as one more
                        // append delta so GE's bubble still gets the links.
                        if did_stream && let Some(sources) = sources_markdown(&v) {
                            frames.push(rpc_frames(json!({
                                "kind": "artifact-update",
                                "taskId": task_for_frames,
                                "contextId": ctx_for_frames,
                                "append": true,
                                "artifact": {
                                    "artifactId": artifact_id,
                                    "parts": [{ "kind": "text", "text": format!("\n\n{sources}") }],
                                },
                            })));
                        }
                        if did_stream {
                            // Close the text artifact before publishing the UI
                            // artifact, even when there were no source links.
                            frames.push(rpc_frames(json!({
                                "kind": "artifact-update",
                                "taskId": task_for_frames,
                                "contextId": ctx_for_frames,
                                "append": true,
                                "lastChunk": true,
                                "artifact": {
                                    "artifactId": artifact_id,
                                    "parts": [{ "kind": "text", "text": "" }],
                                },
                            })));
                        } else {
                            frames.push(rpc_frames(json!({
                                "kind": "artifact-update",
                                "taskId": task_for_frames,
                                "contextId": ctx_for_frames,
                                "lastChunk": true,
                                "artifact": {
                                    "artifactId": artifact_id,
                                    "parts": [{ "kind": "text", "text": reply }],
                                },
                            })));
                        }
                        if let Some(msgs) = a2ui_msgs {
                            frames.push(rpc_frames(json!({
                                "kind": "artifact-update",
                                "taskId": task_for_frames,
                                "contextId": ctx_for_frames,
                                "lastChunk": true,
                                "artifact": {
                                    "artifactId": a2ui_artifact_id,
                                    "parts": triton_core::a2ui::ge::data_parts(msgs),
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
                    triton_core::stream::StreamEvent::Error { error, detail } => {
                        tracing::warn!(task = %task_for_frames, error = %error, "a2a message/stream failed");
                        let text = if detail == Some(USER_TEXT) {
                            error.message()
                        } else {
                            crate::tasks::error_notice(&error)
                        };
                        vec![rpc_frames(json!({
                            "kind": "status-update",
                            "taskId": task_for_frames,
                            "contextId": ctx_for_frames,
                            "status": { "state": "failed", "message": {
                                "kind": "message", "role": "agent",
                                "messageId": uuid::Uuid::new_v4().to_string(),
                                "parts": [{ "kind": "text", "text": text }],
                            } },
                            "final": true,
                        }))]
                    }
                };
                futures::stream::iter(out)
            }
        })
        .flatten();

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

/// Deep-search a `render_report` result for a Vega-Lite chart spec. Peacock
/// exposes the composed specs at `_meta.vega_specs` (an array); older/other
/// shapes carry a `{"kind":"vega","spec":{…}}` component. Search recursively for
/// either — the same deep-search shape as googlechat's `png_base64` finder.
fn find_vega_spec(v: &Value) -> Option<Value> {
    match v {
        Value::Object(m) => {
            if m.get("kind").and_then(Value::as_str) == Some("vega")
                && let Some(spec) = m.get("spec").filter(|s| s.is_object())
            {
                return Some(spec.clone());
            }
            // Peacock's `_meta.vega_specs`: first non-empty object spec.
            if let Some(spec) = m
                .get("vega_specs")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find(|s| s.is_object()))
            {
                return Some(spec.clone());
            }
            m.values().find_map(find_vega_spec)
        }
        Value::Array(a) => a.iter().find_map(find_vega_spec),
        // MCP tool results carry the surface as a JSON STRING in
        // `content[].text`; descend into anything that parses as JSON.
        Value::String(s) if s.trim_start().starts_with(['{', '[']) => {
            serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|parsed| find_vega_spec(&parsed))
        }
        _ => None,
    }
}

/// If `result`'s surface has a `report` component, dispatch `render_report` to
/// the peacock upstream, extract its native Vega-Lite spec, and stamp it onto
/// the report component as `vega_spec` — so the A2UI builder emits an
/// interactive GE `VegaChart` instead of the static PNG. Best-effort: any miss
/// (no report, peacock unreachable, no vega in the render) leaves `result`
/// untouched and the card falls back to the image.
async fn inject_report_vega(
    dispatcher: &Dispatcher,
    result: &mut Value,
    principal: &triton_core::Principal,
    overrides: &triton_core::ContextOverrides,
) {
    let target = result
        .get("surface")
        .and_then(|s| s.get("components"))
        .and_then(Value::as_array)
        .and_then(|cs| {
            cs.iter().enumerate().find_map(|(i, c)| {
                if c.get("kind").and_then(Value::as_str) != Some("report")
                    || c.get("vega_spec").is_some()
                {
                    return None;
                }
                let rid = c.get("report_id").and_then(Value::as_str)?.to_string();
                let params = c
                    .get("args")
                    .and_then(|a| a.get("params"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Some((i, rid, params))
            })
        });
    let Some((idx, report_id, params)) = target else {
        return;
    };
    let args = json!({ "report_id": &report_id, "params": params });
    let Ok(rep) = dispatcher
        .invoke_with(
            "render_report",
            args,
            principal.clone(),
            "a2a",
            overrides.clone(),
        )
        .await
    else {
        return;
    };
    let Some(spec) = find_vega_spec(&rep.result) else {
        return;
    };
    if let Some(comp) = result
        .get_mut("surface")
        .and_then(|s| s.get_mut("components"))
        .and_then(Value::as_array_mut)
        .and_then(|cs| cs.get_mut(idx))
    {
        comp["vega_spec"] = spec;
    }
}

/// Pull human-readable text out of whatever the tool returned. Triton
/// tools answer with an A2UI surface; a spec-A2A caller asked for
/// `text/plain`, so the surface's text is what it gets, and the whole
/// JSON only as a last resort (better than an empty reply).
/// Render a surface's `sources` as one clickable-Markdown-links line
/// ("Sources: [a](url) · [b](url)"), or `None` if there are no http(s) sources.
///
/// The A2UI card (Gemini Enterprise) CANNOT hyperlink — its basic catalog has
/// no link component and its Text excludes link markdown — so sources ride the
/// prose bubble, where GE (and Copilot Studio / Gemini) render Markdown links as
/// real anchors. A `ui://` MCP resource can't open and is skipped.
/// Deep-search a `render_report` result for the document render's structured
/// content: the object carrying an `instances` or `document` key. Robust to
/// the exact nesting (top level, or under `structuredContent`).
fn find_document_structured(v: &Value) -> Option<&Value> {
    match v {
        Value::Object(m) => {
            if m.contains_key("instances") || m.contains_key("document") {
                return Some(v);
            }
            m.values().find_map(find_document_structured)
        }
        Value::Array(a) => a.iter().find_map(find_document_structured),
        _ => None,
    }
}

/// The agent Message `parts` for a source "Open" click. Default: render the
/// peacock `document` report as a **native Material** surface (GA widgets, no
/// allowlist). When `TRITON_GE_DOC_IFRAME` is set and a signed `/docs` URL is
/// present: the Canvas side-panel + `IFrameUrl` variant. A short text part
/// leads for text-only clients.
async fn doc_open_parts(
    dispatcher: &Dispatcher,
    principal: &triton_core::Principal,
    doc: &triton_core::a2ui::ge::OpenDocRef<'_>,
    overrides: &triton_core::ContextOverrides,
) -> Vec<Value> {
    use triton_core::a2ui::ge;
    let msgs = if ge::doc_iframe_enabled() && !doc.url.is_empty() {
        ge::build_document_canvas(doc.url, doc.label)
    } else if !doc.skill.is_empty() && !doc.id.is_empty() {
        // Native Material render: dispatch the peacock `document` report and
        // map its structuredContent to Material components.
        let args =
            json!({ "report_id": "document", "params": { "skill": doc.skill, "id": doc.id } });
        let structured = match dispatcher
            .invoke_with(
                "render_report",
                args,
                principal.clone(),
                "a2a",
                overrides.clone(),
            )
            .await
        {
            Ok(rep) => find_document_structured(&rep.result)
                .cloned()
                .unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        ge::build_document_material(&structured, doc.label)
    } else {
        // No skill/id (an unexpected source label). Do NOT fall through to the
        // Canvas/IFrameUrl here: in native-default mode (the common case)
        // IFrameUrl needs GE's widget allowlist and would show a security
        // error the operator never opted into. Return the signed /docs link as
        // plain text so the user can still open it (or an unavailable note).
        let text = if !doc.url.is_empty() {
            format!("Open the document: {}", doc.url)
        } else {
            "This document is unavailable.".to_string()
        };
        return vec![json!({ "kind": "text", "text": text })];
    };
    let mut parts = vec![json!({
        "kind": "text",
        "text": format!("Opening “{}”.", doc.label),
    })];
    parts.extend(ge::data_parts(msgs));
    parts
}

pub(crate) fn sources_markdown(result: &Value) -> Option<String> {
    let components = result.get("surface")?.get("components")?.as_array()?;
    let links: Vec<String> = components
        .iter()
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("sources"))
        .filter_map(|c| c.get("items").and_then(Value::as_array))
        .flatten()
        .filter_map(|it| {
            let url = it
                .get("resource")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http"))?;
            let label = it.get("label").and_then(Value::as_str).unwrap_or("source");
            Some(format!("[{label}]({url})"))
        })
        .collect();
    (!links.is_empty()).then(|| format!("Sources: {}", links.join(" · ")))
}

/// The text part of a reply (say it once): beside a rendered A2UI surface,
/// the result's one-sentence `summary` when it has one, because the rows,
/// choices and facts are in the components; with no surface shown, the full
/// [`reply_text`].
pub fn text_part(result: &Value, surface_shown: bool) -> String {
    if surface_shown
        && let Some(summary) = result
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return summary.to_string();
    }
    reply_text(result)
}

pub fn reply_text(result: &Value) -> String {
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
            // Sources as clickable Markdown links (see `sources_markdown`).
            if let Some(sources) = sources_markdown(result) {
                if !joined.is_empty() {
                    joined.push_str("\n\n");
                }
                joined.push_str(&sources);
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

fn tasks_get(
    state: SpecState,
    req: RpcRequest,
    principal: &triton_core::principal::Principal,
) -> Response {
    let Some(id) = req.params.get("id").and_then(Value::as_str) else {
        return rpc_error(&req.id, INVALID_PARAMS, "params.id is required");
    };
    // #306 crew F1: a task id IS a trace id; only its owner (or a caller
    // with a visible audit entry for the trace, as for `/v1/trace`) reads
    // it, and "someone else's task" looks exactly like "no such task".
    // A task left working by a previous process is resumed here.
    match crate::a2a_v1::readable_live(&state.a2a, principal, id) {
        Ok((rec, _)) => rpc_ok(&req.id, crate::a2a_v1::task_v03(&rec)),
        Err(e) => crate::a2a_v1::rpc_error_v03(&req.id, &e),
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

    #[test]
    fn reply_text_appends_sources_as_markdown_links() {
        // http(s) sources become clickable Markdown links in the prose bubble
        // (the GE card cannot hyperlink); a ui:// resource is skipped.
        let result = serde_json::json!({
            "surface": { "components": [
                { "kind": "text", "value": "Initech leads." },
                { "kind": "sources", "items": [
                    { "label": "followups", "resource": "https://agent.example/docs/A" },
                    { "label": "query", "resource": "https://agent.example/docs/B" },
                    { "label": "ui-only", "resource": "ui://peacock/x" }
                ] }
            ] }
        });
        assert_eq!(
            reply_text(&result),
            "Initech leads.\n\nSources: [followups](https://agent.example/docs/A) · [query](https://agent.example/docs/B)"
        );

        // No sources → no trailing line.
        let none = serde_json::json!({
            "surface": { "components": [ { "kind": "text", "value": "Initech leads." } ] }
        });
        assert_eq!(reply_text(&none), "Initech leads.");
    }

    #[test]
    fn find_document_structured_locates_the_render_payload() {
        // A render_report(document) result nests structuredContent; the helper
        // finds the object carrying instances/document at any depth.
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": "…" }],
            "structuredContent": {
                "document": { "skill": "account", "id": "beverages" },
                "instances": { "self": { "facts": [] } },
            },
            "_meta": { "ui": {} },
        });
        let found = find_document_structured(&result).expect("finds it");
        assert_eq!(found["document"]["id"], "beverages");
        // A result with neither key yields None.
        assert!(find_document_structured(&serde_json::json!({ "rows": [] })).is_none());
    }
}

#[cfg(test)]
mod agent_slug_tests {
    use super::validate_agent_slug;

    #[test]
    fn roster_style_slugs_are_accepted() {
        for ok in ["sales", "supply-planner", "agent2", "a"] {
            assert!(validate_agent_slug(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn slugs_that_could_collide_with_base_routes_are_rejected() {
        // Every existing route under the `/a2a` nest contains `:` or `.`.
        for bad in [
            "message:send",
            ".well-known",
            "well-known",
            "message",
            "tasks",
            "",
            "A",
            "a_b",
            "a/b",
            "-a",
            "a-",
            &"x".repeat(65),
        ] {
            assert!(validate_agent_slug(bad).is_err(), "`{bad}`");
        }
    }
}
