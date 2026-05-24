//! v0.2 Telegram adapter — webhook inbound half.
//!
//! Scope of PR 13: receive a Telegram `Update`, verify the
//! `X-Telegram-Bot-Api-Secret-Token` header in constant time
//! (FR-I-8 / M-SIG-1), resolve the platform sender to a
//! [`Principal`] via the manifest's `sender_table` strategy
//! (FR-I-7 / M-IDENT-1), dispatch through the same
//! [`triton_core::Dispatcher`] every protocol goes through, and
//! emit the FR-AU-1 `phase: dispatch` audit line tagged
//! `protocol: messenger:telegram`.
//!
//! The outbound courier (posting back to api.telegram.org with
//! the rendered surface) lands in PR 14 alongside the L6′ surface
//! mapper.

mod surface_mapper;
pub use surface_mapper::RenderedMessage;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use triton_core::{Dispatcher, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

const PROTOCOL: &str = "messenger:telegram";
const HEADER_SECRET: &str = "X-Telegram-Bot-Api-Secret-Token";
/// Upper bound on the configured secret_token length. Telegram's
/// API documents the secret as 1–256 chars; we pin the constant-time
/// comparator to a fixed scratch buffer of this size so neither the
/// configured length nor the presented length is observable from
/// handler latency (FR-I-8).
const MAX_SECRET_TOKEN: usize = 256;

/// Per-Telegram-user claims resolved from the `sender_table`. The
/// table is a JSON object keyed by Telegram user id (as a string)
/// to keep the manifest format human-editable.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// Configuration for the outbound courier half. Default base is
/// `https://api.telegram.org`; tests override it via
/// `TRITON_TELEGRAM_API_BASE` to point at a local fake.
#[derive(Debug, Clone)]
pub struct CourierConfig {
    pub api_base: String,
    pub timeout: std::time::Duration,
}

impl Default for CourierConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.telegram.org".to_string(),
            timeout: std::time::Duration::from_secs(10),
        }
    }
}

/// Build artefacts the adapter holds. Constructed once at boot
/// from the manifest entry; immutable after that.
pub struct TelegramAdapter {
    name: String,
    secret_token: String,
    bot_token: String,
    correlation_key: Vec<u8>,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
    courier: CourierClient,
}

struct CourierClient {
    base: String,
    http: reqwest::Client,
}

impl TelegramAdapter {
    /// Read a manifest [`Adapter`] of `kind: telegram`, resolve
    /// every declared credential through the supplied
    /// [`SecretResolver`] (literal or Vault KV v2), and produce a
    /// runnable adapter. Called once at boot per declared adapter.
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        courier_config: CourierConfig,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::Telegram {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::SecretToken {
            return Err(BuildError::Unsupported(format!(
                "PR 13 only handles `signature: secret_token`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "PR 13 only handles `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let secret_field = adapter
            .inbound
            .credentials
            .get("secret")
            .ok_or(BuildError::MissingCredential("inbound.secret"))?;
        let secret_token = resolver
            .resolve(secret_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.secret", e))?;
        if secret_token.is_empty() || secret_token.len() > MAX_SECRET_TOKEN {
            return Err(BuildError::Unsupported(format!(
                "inbound.secret length must be 1..={MAX_SECRET_TOKEN} bytes, got {}",
                secret_token.len()
            )));
        }

        let table_field = adapter
            .identity
            .credentials
            .get("table")
            .ok_or(BuildError::MissingCredential("identity.table"))?;
        let table_json = resolver
            .resolve(table_field)
            .await
            .map_err(|e| BuildError::Resolve("identity.table", e))?;
        let sender_table: HashMap<String, SenderClaims> =
            serde_json::from_str(&table_json).map_err(|e| BuildError::TableParse(e.to_string()))?;

        // FR-L-6 / NFR-S-5: every credential field MUST resolve at
        // boot or the binary refuses to start. `outbound.token` is
        // consumed by PR 18's courier (the bot's API path token —
        // see `https://api.telegram.org/bot{token}/sendMessage`).
        // `correlation_key` isn't functionally consumed yet (PR
        // 18+: HMAC correlation tokens), but it still resolves at
        // boot so a bad Vault ref fails closed.
        let bot_token = match adapter.outbound.credentials.get("token") {
            Some(field) => resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.token", e))?,
            None => return Err(BuildError::MissingCredential("outbound.token")),
        };
        let correlation_key = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?
            .into_bytes();

        let courier = CourierClient::new(courier_config)?;
        Ok(Self {
            name: name.to_string(),
            secret_token,
            bot_token,
            correlation_key,
            sender_table,
            dispatcher,
            courier,
        })
    }

    /// Mount the inbound webhook at `/<adapter-name>/webhook`.
    pub fn router(self: Arc<Self>) -> Router {
        let name = self.name.clone();
        let path = format!("/{name}/webhook");
        Router::new()
            .route(&path, post(handle_webhook))
            .with_state(self)
    }
}

impl CourierClient {
    fn new(cfg: CourierConfig) -> Result<Self, BuildError> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| BuildError::Unsupported(format!("courier http client: {e}")))?;
        Ok(Self {
            base: cfg.api_base.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// POST `<base>/bot{token}/sendMessage` with `{chat_id, text}`.
    ///
    /// **Token-leak guard (FR-AU-3 / Codex PR 18 blocker).** The
    /// bot token appears in the URL path. `reqwest::Error::Display`
    /// often includes the request URL, so the raw error MUST NOT
    /// flow into the audit/log pipeline. We sanitise every error
    /// string through [`redact_url`] before returning so the
    /// caller's `tracing::warn!(error = %e)` cannot leak the token
    /// even by accident.
    ///
    /// **Body-shape guard (Codex PR 18 blocker).** Telegram's Bot
    /// API returns `200 OK` with `{ok: false, error_code,
    /// description, parameters: {retry_after}}` on a successful
    /// HTTP roundtrip that nonetheless failed at the application
    /// layer (rate limit, bad chat_id, blocked-by-user, ...).
    /// Treating any 2xx as success would silently lose those
    /// failures. We parse the envelope and require `ok: true`.
    async fn send_message_body(
        &self,
        bot_token: &str,
        body: &Value,
    ) -> Result<SendOutcome, CourierError> {
        let url = format!("{}/bot{}/sendMessage", self.base, bot_token);
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| CourierError::Transport(redact_url(&e.to_string(), bot_token)))?;
        let http_status = resp.status().as_u16();
        let body: BotApiEnvelope = resp
            .json()
            .await
            .map_err(|e| CourierError::Decode(redact_url(&e.to_string(), bot_token)))?;
        if body.ok {
            return Ok(SendOutcome {
                http_status,
                label: PostLabel::Posted,
            });
        }
        // ok:false. Classify by error_code: 429 (or 4xx with a
        // retry_after) is a retry-eligible class; other 4xx are
        // permanent drops; >= 500 is retry (transient upstream).
        let retry_after = body.parameters.and_then(|p| p.retry_after);
        let code = body.error_code.unwrap_or(0);
        let label = if code == 429 || retry_after.is_some() || code >= 500 {
            PostLabel::Retry
        } else {
            PostLabel::Dropped
        };
        Err(CourierError::Application {
            http_status,
            label,
            error_code: code,
        })
    }
}

#[derive(Debug)]
struct SendOutcome {
    http_status: u16,
    label: PostLabel,
}

/// FR-AU-1 v0.2 closed set for the chat post audit's `status_label`.
#[derive(Debug, Clone, Copy)]
pub enum PostLabel {
    /// Bot API accepted the message (`{ok: true}` on the response).
    Posted,
    /// Bot API said retry-eligible (429, 5xx, explicit retry_after).
    Retry,
    /// Bot API said permanent failure (e.g. 400 invalid chat_id,
    /// 403 blocked by user). Not retried; surfaced to operator.
    Dropped,
}

impl PostLabel {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Posted => "posted",
            Self::Retry => "retry",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug)]
enum CourierError {
    Transport(String),
    Decode(String),
    Application {
        http_status: u16,
        label: PostLabel,
        error_code: i64,
    },
}

impl CourierError {
    fn label(&self) -> PostLabel {
        match self {
            Self::Transport(_) | Self::Decode(_) => PostLabel::Retry,
            Self::Application { label, .. } => *label,
        }
    }
    fn http_status(&self) -> u16 {
        match self {
            Self::Transport(_) | Self::Decode(_) => 0,
            Self::Application { http_status, .. } => *http_status,
        }
    }
    fn message(&self) -> String {
        match self {
            Self::Transport(m) => format!("telegram courier transport: {m}"),
            Self::Decode(m) => format!("telegram courier decode: {m}"),
            Self::Application {
                error_code, label, ..
            } => format!(
                "telegram courier application: error_code={error_code}, label={}",
                label.as_str()
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BotApiEnvelope {
    ok: bool,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    parameters: Option<BotApiResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct BotApiResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

/// Strip every occurrence of `/bot{token}` from an error/log string.
/// Belt-and-braces: even if `reqwest` ever includes the URL in its
/// own error Display, the bot token never reaches the audit pivot.
fn redact_url(s: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        return s.to_string();
    }
    s.replace(bot_token, "<redacted>")
}

/// Errors building the adapter from a manifest entry. Every
/// variant is fatal at boot — a misconfigured deploy exits the
/// binary non-zero so the substrate sees the failure clearly
/// (M-SECRETS-1 / FR-L-4). PR 13's `VaultUnsupported` warn-and-skip
/// carve-out was lifted in PR 16 once the resolver landed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: telegram`")]
    WrongKind,
    #[error("PR 13 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    #[serde(default)]
    message: Option<TelegramMessage>,
    /// Inbound interaction with an inline_keyboard button. Carries
    /// the HMAC correlation token in `data`; the inbound handler
    /// verifies the signature and re-dispatches the recovered
    /// (tool, args) pair as a fresh call (PR 21).
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    from: TelegramUser,
    chat: TelegramChat,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    from: TelegramUser,
    message: Option<TelegramMessage>,
    /// The correlation token the surface mapper put in
    /// `callback_data` when it rendered the button. Empty/absent on
    /// callback queries triggered by non-data buttons (game URLs,
    /// login URLs); we treat those as malformed since the mapper
    /// only emits data buttons.
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    /// Telegram `chat.id` is the post-back target. For private
    /// chats this equals `from.id`; for group chats it's the group
    /// id (negative integer). Always-i64 because Telegram uses
    /// negative values for groups.
    id: i64,
}

async fn handle_webhook(
    State(adapter): State<Arc<TelegramAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Signature check FIRST on raw bytes (FR-I-8 / M-SIG-1). The
    // body extractor is `Bytes`, not `Json`, so we never parse JSON
    // for an unauthenticated request — that closes the parse-then-
    // reject audit gap Codex flagged in PR 13 review.
    let presented = headers
        .get(HEADER_SECRET)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), adapter.secret_token.as_bytes()) {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth("bad secret token".into()),
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // Only after the secret check do we trust the body enough to
    // parse it. A malformed body from an authenticated sender is a
    // Telegram bug; ack with 400 and audit as validation rather than
    // 5xx (Telegram retries on 5xx — we don't want a loop).
    let update: TelegramUpdate = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed update body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed update").into_response();
        }
    };

    // PR 21: callback_query path (button click). The token in
    // `data` is HMAC-verified against the adapter's correlation_key
    // and the recovered (tool, args) is dispatched as a fresh
    // call. The principal still comes from the sender (sender_table
    // lookup of `from.id`); the token MUST NOT decide the
    // principal, only the tool+args pair.
    if let Some(cq) = update.callback_query {
        return handle_callback_query(&adapter, cq).await;
    }

    let Some(message) = update.message else {
        // Updates without a `message` (edited messages, channel
        // posts, etc.) are silently 200'd in PR 13 — Telegram
        // retries on non-2xx, so the safest behaviour for a kind
        // we don't yet handle is "accept and ignore".
        return StatusCode::OK.into_response();
    };
    let Some(text) = message.text.as_ref().filter(|t| !t.is_empty()) else {
        return StatusCode::OK.into_response();
    };

    // FR-I-7 sender resolution. Unknown sender → rejected audit
    // + 401 (we treat unrecognised platform users as auth failures
    // because there's no in-band path for them to acquire a
    // Triton identity yet).
    let sender_key = message.from.id.to_string();
    let Some(claims) = adapter.sender_table.get(&sender_key) else {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {sender_key}")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

    // FR-I-6 / M-IDENT-1: structurally identical Principal to the
    // OIDC-derived one (`raw_token` is empty here — chat platforms
    // don't carry a JWT we forward; the upstream router won't
    // touch this field for messenger-routed calls).
    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };

    // PR 19 minimal command parser: `/<tool> <rest>` routes to
    // `tool` with `{ subject: rest }` (matches `narrate`'s arg
    // shape); anything else falls through to `echo` with the
    // whole text as `{ message: text }`. The real shared command
    // parser ships when the second adapter (Discord) needs it; for
    // now this lets PR 19 exercise both tool shapes through the
    // surface mapper.
    let (tool_name, args) = route_command(text);
    let tool_name: &str = tool_name;
    let chat_id = message.chat.id;
    let principal_for_post = principal.clone();
    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => {
            match render_dispatch_result(&dispatch.result, &adapter.correlation_key) {
                Ok(rendered) => {
                    if rendered.deferred_buttons > 0 {
                        // PR 19 doesn't render buttons (need HMAC
                        // correlation tokens in the next PR).
                        // Logged — counted, not silent — so the
                        // operator sees the gap until couriers can
                        // carry buttons through.
                        tracing::warn!(
                            tool = tool_name,
                            deferred_buttons = rendered.deferred_buttons,
                            "telegram surface mapper: button components deferred until correlation-token PR",
                        );
                    }
                    if rendered.truncated {
                        // Codex PR 19 blocker 2 follow-up: log
                        // every truncation event so operators can
                        // tune tool outputs before users complain.
                        tracing::warn!(
                            tool = tool_name,
                            cap_bytes = surface_mapper::TELEGRAM_TEXT_MAX_BYTES,
                            "telegram surface mapper: rendered text exceeded cap; truncated",
                        );
                    }
                    post_back(&adapter, &principal_for_post, tool_name, chat_id, rendered).await;
                    StatusCode::OK.into_response()
                }
                Err(surface_mapper::RenderError::EmptyAfterRender) => {
                    // Codex PR 19 blocker 1: empty / button-only
                    // surface used to ship `text: ""` and let
                    // Telegram 400. Now we refuse at the mapper
                    // edge and audit the courier as `dropped` so
                    // the trace is visible to the substrate audit
                    // collector — no wasted API call, no retry.
                    tracing::warn!(
                        tool = tool_name,
                        "telegram surface mapper: empty surface (no renderable components); skipping post-back",
                    );
                    let provider =
                        TritonError::Provider("telegram surface mapper: empty surface".into());
                    adapter.dispatcher.record_post(
                        tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        0,
                        Err((&provider, 0, "dropped")),
                    );
                    StatusCode::OK.into_response()
                }
            }
        }
        Err(e) => {
            // Dispatcher already audited the failure. Map to a
            // status that won't trigger Telegram retry storms for
            // permanent app-layer failures: validation/auth-shaped
            // errors are acked 200 (Telegram has nothing useful to
            // retry), and only transient provider faults / open
            // circuits earn a retryable 5xx.
            tracing::warn!(error = %e, class = %e.class(), "telegram tool dispatch failed");
            telegram_status_for(&e).into_response()
        }
    }
}

/// Handle a Telegram `callback_query` update. The token in
/// `data` is HMAC-verified against the adapter's `correlation_key`
/// and the recovered `(tool, args)` re-dispatched. Per
/// architecture.md §8.7 the platform never sees the tool name or
/// args directly; we only ever ship the signed token through
/// Telegram and trust nothing it sends back beyond the HMAC.
async fn handle_callback_query(
    adapter: &Arc<TelegramAdapter>,
    cq: TelegramCallbackQuery,
) -> Response {
    // FR-I-7 sender resolution. The principal comes from `from.id`
    // — never from the token. A hostile platform actor cannot
    // impersonate a different user by forging tokens because the
    // sender_table lookup is independent.
    let sender_key = cq.from.id.to_string();
    let Some(claims) = adapter.sender_table.get(&sender_key) else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {sender_key}")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

    let Some(token) = cq.data.as_deref().filter(|s| !s.is_empty()) else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("callback_query without data".into()),
        );
        return (StatusCode::BAD_REQUEST, "callback_query missing data").into_response();
    };

    let (tool_name, args) = match triton_correlation::decode(token, &adapter.correlation_key) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Auth(format!("callback token: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "callback token rejected").into_response();
        }
    };

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };

    // For private chats the post-back chat_id equals `from.id`;
    // group chats embed it on `callback_query.message.chat.id`.
    // Fall back to `from.id` when the platform omits the parent
    // message (rare but legal for inline-mode callbacks).
    let chat_id = cq
        .message
        .as_ref()
        .map(|m| m.chat.id)
        .unwrap_or(cq.from.id as i64);

    let principal_for_post = principal.clone();
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => match render_dispatch_result(&dispatch.result, &adapter.correlation_key) {
            Ok(rendered) => {
                post_back(adapter, &principal_for_post, &tool_name, chat_id, rendered).await;
                StatusCode::OK.into_response()
            }
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                tracing::warn!(
                    tool = tool_name,
                    "telegram callback: empty surface; skipping post-back",
                );
                let provider =
                    TritonError::Provider("telegram surface mapper: empty surface".into());
                adapter.dispatcher.record_post(
                    &tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Err((&provider, 0, "dropped")),
                );
                StatusCode::OK.into_response()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "telegram callback dispatch failed");
            telegram_status_for(&e).into_response()
        }
    }
}

fn route_command(text: &str) -> (&'static str, Value) {
    if let Some(rest) = text.strip_prefix('/') {
        // Split on the first space; if there's no space the whole
        // remainder is the command, with empty args. PR 19's
        // original `split_once.is_some()` made `/narrate` (no
        // space) silently fall through to echo — Codex flagged
        // that as surprising. Now `/narrate` with no args routes
        // to narrate with an empty subject; the tool decides what
        // to do with it (echoes back "Hello, .").
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        match tool {
            "narrate" => return ("narrate", json!({ "subject": subject })),
            // Dev-only command, gated on the same feature as the
            // dev-only `EmptySurface` tool itself. Without the gate
            // a production build would reserve `/empty` and route
            // it to an unregistered tool, producing a confusing
            // "unknown tool" instead of the user's text echoing
            // back. Codex PR 20 review caught this gap.
            #[cfg(feature = "dev-token")]
            "empty" => return ("empty_surface", json!({})),
            _ => {
                // Unknown commands fall through to echo so the
                // user sees their raw text and knows the command
                // wasn't recognised.
            }
        }
    }
    ("echo", json!({ "message": text }))
}

fn render_dispatch_result(
    result: &serde_json::Value,
    correlation_key: &[u8],
) -> Result<RenderedMessage, surface_mapper::RenderError> {
    // Tools that emit an A2UI surface route through the mapper.
    // Everything else falls back to PR 18's bare-text path so the
    // echo-shaped `{ "echo": "..." }` reply still works without
    // forcing every tool into the A2UI envelope.
    if let Some(r) = surface_mapper::try_render_surface(result, correlation_key) {
        return r;
    }
    let text = if let Some(obj) = result.as_object()
        && obj.len() == 1
        && let Some(s) = obj.values().next().and_then(|v| v.as_str())
    {
        s.to_string()
    } else if let Some(s) = result.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(result).unwrap_or_else(|_| "<unrenderable>".to_string())
    };
    if text.is_empty() {
        return Err(surface_mapper::RenderError::EmptyAfterRender);
    }
    Ok(RenderedMessage {
        text,
        parse_mode: None,
        reply_markup: None,
        deferred_buttons: 0,
        truncated: false,
    })
}

async fn post_back(
    adapter: &TelegramAdapter,
    principal: &Principal,
    tool_name: &str,
    chat_id: i64,
    msg: RenderedMessage,
) {
    let body = surface_mapper::build_send_message_body(chat_id, &msg);
    let start = std::time::Instant::now();
    let outcome = adapter
        .courier
        .send_message_body(&adapter.bot_token, &body)
        .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match outcome {
        Ok(send) => {
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Ok((send.http_status, send.label.as_str())),
            );
        }
        Err(e) => {
            // `e.message()` is already sanitized (bot token stripped
            // by `redact_url`) before it reaches this branch, so
            // logging + auditing it cannot leak the token per FR-AU-3.
            let label = e.label();
            let http_status = e.http_status();
            let msg = e.message();
            tracing::warn!(
                courier_label = label.as_str(),
                "telegram courier failed: {msg}"
            );
            let provider = TritonError::Provider(msg);
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, http_status, label.as_str())),
            );
        }
    }
}

/// Map a dispatcher error to a Telegram-friendly status. We want to
/// avoid retry storms for permanent app-layer failures: Telegram
/// retries non-2xx for ~24 h, which would replay the same broken
/// update endlessly. Permanent failures (validation, auth) are
/// acked 200 — the message *was* received and decided — and only
/// transient infra faults earn a retryable 5xx.
fn telegram_status_for(e: &TritonError) -> StatusCode {
    if e.is_circuit_open() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    if e.is_tool_timeout() {
        return StatusCode::GATEWAY_TIMEOUT;
    }
    match e {
        TritonError::Provider(_) => StatusCode::BAD_GATEWAY,
        TritonError::Validation(_) | TritonError::Auth(_) | TritonError::Tool(_) => StatusCode::OK,
    }
}

fn record_rejection(adapter: &TelegramAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        // We don't know the tool until after sender resolution, so
        // for boundary rejections we use the adapter name as the
        // tool label.
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Constant-time across BOTH content and length (FR-I-8).
    //
    // The naive `if a.len() != b.len() { return false }` short-
    // circuits before the ct_eq, so an attacker controlling the
    // presented header can bisect the configured secret's length by
    // measuring handler latency. Codex flagged this in PR 13 review.
    //
    // Fix: copy both sides into fixed-size scratch buffers
    // (zero-padded to MAX_SECRET_TOKEN) and run ct_eq over the full
    // scratch length. The boot-time bound on `secret_token.len()`
    // guarantees the configured side fits; the presented side is
    // truncated past MAX which is fine — anything that long is
    // already not the configured secret. Folding the length-equality
    // bit into the final result keeps the function total.
    let mut a_buf = [0u8; MAX_SECRET_TOKEN];
    let mut b_buf = [0u8; MAX_SECRET_TOKEN];
    let a_n = a.len().min(MAX_SECRET_TOKEN);
    let b_n = b.len().min(MAX_SECRET_TOKEN);
    a_buf[..a_n].copy_from_slice(&a[..a_n]);
    b_buf[..b_n].copy_from_slice(&b[..b_n]);
    let content_eq: bool = a_buf.ct_eq(&b_buf).into();
    let length_eq = a.len() == b.len();
    content_eq & length_eq
}
