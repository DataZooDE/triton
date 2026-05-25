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

// Exposed (pub) so `triton-adapters-http`'s `/v1/surface/render`
// route can ask the same mapper the Telegram courier uses. The
// adapter binary doesn't change — only the visibility widens.
pub mod surface_mapper;
pub use surface_mapper::RenderedMessage;

// PR 32: numbered_prompts state machine for `Component::Form`
// rendering. Pure logic, no I/O — tested via unit tests in the
// module + driven end-to-end by `crates/triton-tests/tests/
// telegram_form.rs`.
pub mod form_state;

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
use triton_rasterizer::{Client as RasterizerClient, DashboardRequest, RasterizerError};
use triton_secrets::{ResolveError, SecretResolver};

const PROTOCOL: &str = "messenger:telegram";
const HEADER_SECRET: &str = "X-Telegram-Bot-Api-Secret-Token";
/// Upper bound on the configured secret_token length. Telegram's
/// API documents the secret as 1–256 chars; we pin the constant-time
/// comparator to a fixed scratch buffer of this size so neither the
/// configured length nor the presented length is observable from
/// handler latency (FR-I-8).
const MAX_SECRET_TOKEN: usize = 256;
/// PR 23: how stale a `callback_query.message.date` is allowed to
/// be before we reject the click as a replayed/stale button.
/// 5 minutes matches Discord's PR 22 timestamp-skew window and
/// the platform pattern most chat bots adopt for button TTLs.
const CALLBACK_TTL_SECS: u32 = 300;

/// PR 23: small allowance for a platform clock running ahead of
/// ours. Anything beyond this is treated as an attempt to extend
/// the TTL by pre-dating the button. 60 s covers normal NTP drift
/// while still bounding any clock-skew attack to one minute of
/// forward staleness (Codex PR 23 concern).
const CALLBACK_FUTURE_SKEW_SECS: u32 = 60;

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    /// PR 36: optional handle to the out-of-process rasterizer
    /// service (FR-A-11). `None` when no rasterizer URL was
    /// configured — Dashboard components then fall back to the
    /// pre-PR-36 deferred-text path so users still see something.
    rasterizer: Option<RasterizerClient>,
    /// PR 24: per-adapter rate limit (NFR-P-3 first tier).
    /// Consumed BEFORE body parse or sender lookup so a noisy
    /// bot — or attacker who has the secret token — can't
    /// saturate the dispatcher.
    rate_limit: triton_core::ratelimit::TokenBucket,
    /// PR 28: per-tenant rate limit (NFR-P-3 second tier).
    /// Consumed AFTER sender resolution; sized from the same
    /// manifest `rate_limit` values so one tenant can't starve
    /// others sharing the same adapter quota.
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    /// PR 32: per-chat numbered-prompts state for `Component::Form`
    /// surfaces. In-memory only (G-8). Capped per-tenant to avoid
    /// OOM from a noisy tenant; oldest in-flight form is evicted
    /// when the cap is hit. See `form_state::FormStateStore`.
    form_state: form_state::FormStateStore,
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
    ///
    /// `rasterizer` (PR 36) is the optional out-of-process
    /// dashboard renderer. `None` falls back to a deferred-text
    /// placeholder for `Component::Dashboard` (same shape as PR
    /// 27); `Some(client)` ships rasterised PNGs via `sendPhoto`.
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        courier_config: CourierConfig,
        rasterizer: Option<RasterizerClient>,
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
        // PR 28: the adapter-wide bucket is the DoS-floor guard
        // (consumed before sender resolution); the per-tenant
        // bucket is the fair-share gate (consumed after). For the
        // per-tenant gate to add value over the adapter-wide one,
        // the adapter-wide bucket needs headroom — otherwise it
        // fires first on every burst and the per-tenant logic
        // never gets to differentiate. Multiplying by 10 gives
        // enough room for ~10 active tenants at the manifest's
        // per-tenant cap before the adapter-wide DoS ceiling
        // engages. Operators tune the manifest values per-tenant;
        // the 10x multiplier is the implicit headroom budget.
        const ADAPTER_HEADROOM: u32 = 10;
        let rate_limit = triton_core::ratelimit::TokenBucket::new(
            adapter
                .rate_limit
                .messages_per_sec
                .saturating_mul(ADAPTER_HEADROOM),
            adapter.rate_limit.burst.saturating_mul(ADAPTER_HEADROOM),
        );
        let per_tenant_limit = triton_core::ratelimit::PerTenantBuckets::new(
            adapter.rate_limit.messages_per_sec,
            adapter.rate_limit.burst,
        );
        // PR 32: per-tenant cap on in-flight numbered-prompts
        // forms. Production keeps the default. The integration
        // test that exercises the LRU-eviction path drops this to
        // 2 via `TRITON_TELEGRAM_FORM_CAP_PER_TENANT` so it doesn't
        // need to install 100 forms to reach the eviction branch.
        // Parsing failures fall back to the default — env vars
        // shouldn't take the binary down.
        let form_cap = std::env::var("TRITON_TELEGRAM_FORM_CAP_PER_TENANT")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(form_state::DEFAULT_PER_TENANT_CAP);
        Ok(Self {
            name: name.to_string(),
            secret_token,
            bot_token,
            correlation_key,
            sender_table,
            dispatcher,
            courier,
            rasterizer,
            rate_limit,
            per_tenant_limit,
            form_state: form_state::FormStateStore::with_cap(form_cap),
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

    /// POST `<base>/bot{token}/sendPhoto` as multipart/form-data
    /// carrying the rendered Dashboard PNG. Used by PR 36's
    /// Dashboard wiring. Same `{ok: true/false}` envelope handling
    /// as [`Self::send_message_body`]; same token-leak redaction
    /// rules.
    async fn send_photo(
        &self,
        bot_token: &str,
        fields: surface_mapper::SendPhotoFields,
        png: Vec<u8>,
    ) -> Result<SendOutcome, CourierError> {
        let url = format!("{}/bot{}/sendPhoto", self.base, bot_token);
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("dashboard.png")
            .mime_str("image/png")
            .map_err(|e| CourierError::Transport(redact_url(&e.to_string(), bot_token)))?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", fields.chat_id.to_string())
            .part("photo", part);
        if let Some(caption) = fields.caption {
            form = form.text("caption", caption);
        }
        if let Some(pm) = fields.parse_mode {
            form = form.text("parse_mode", pm.to_string());
        }
        if let Some(rm) = fields.reply_markup {
            form = form.text("reply_markup", rm);
        }
        let resp = self
            .http
            .post(&url)
            .multipart(form)
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
    /// Unix seconds when the message was sent. For
    /// `callback_query.message`, this is the time the bot SENT
    /// the original message carrying the button — i.e. the
    /// rendering moment. PR 23 uses this as the freshness anchor
    /// for replay protection (architecture.md §8.7's
    /// out-of-scope-in-PR-21 lift).
    #[serde(default)]
    date: u64,
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

    // PR 24: per-adapter rate limit (NFR-P-3). Consumed AFTER the
    // signature check (so attackers can't waste tokens with bogus
    // sigs) but BEFORE body parsing or sender resolution (so a
    // single noisy bot can't bypass by spraying random sender ids).
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "telegram adapter `{}` rate limit hit; retry in {:.2}s",
                adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "rate limited",
        )
            .into_response();
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

    // PR 28: per-tenant fair-share rate limit (NFR-P-3 second
    // tier). Consumed AFTER sender resolution so the bucket key
    // is the verified tenant id, not the platform user id.
    if let Err(retry_after) = adapter.per_tenant_limit.try_take(&claims.tenant) {
        record_rejection(
            &adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::RateLimited(format!(
                "tenant `{}` rate limit hit on adapter `{}`; retry in {:.2}s",
                claims.tenant, adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "tenant rate limited",
        )
            .into_response();
    }

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

    let chat_id = message.chat.id;

    // PR 32: numbered_prompts intercept. Before the normal
    // command parser, check whether there's an active form for
    // this (chat, telegram_user_id). The intercept order is:
    //   1. `/cancel` while a form is active → clear + ack.
    //   2. Active form → feed the message as the next field's
    //      value; on completion dispatch the form's submit tool.
    //   3. No active form → fall through to `route_command`.
    //
    // PR 37 Finding 5: the FormKey carries `telegram_user_id`
    // (platform-asserted `from.id`), NOT the verified `sub`. Two
    // entries in `sender_table` can map distinct user ids to the
    // SAME sub (e.g. cross-tenant duplicates) — keying on the
    // platform id makes the lookup unambiguous.
    let form_key = form_state::FormKey {
        chat_id,
        telegram_user_id: message.from.id as i64,
    };
    let principal_for_post = principal.clone();

    // `/cancel` with active form: drop the slot and reply. No
    // dispatch happens here — only a `phase: post` audit fires
    // (we DO send a sendMessage so the post is real). The audit
    // names the form's submit tool so the operator can correlate
    // the cancel with the form that was abandoned.
    if text.trim() == "/cancel" && adapter.form_state.has_active(&form_key) {
        adapter.form_state.cancel(&form_key);
        let reply = courier_reply("Form cancelled.");
        post_back(&adapter, &principal_for_post, "form_cancel", chat_id, reply).await;
        return StatusCode::OK.into_response();
    }

    // Active form: feed the user's message as the next field.
    if let Some(outcome) = adapter.form_state.advance(&form_key, text) {
        return handle_form_outcome(&adapter, principal_for_post, &form_key, chat_id, outcome)
            .await;
    }

    // PR 19 minimal command parser: `/<tool> <rest>` routes to
    // `tool` with `{ subject: rest }` (matches `narrate`'s arg
    // shape); anything else falls through to `echo` with the
    // whole text as `{ message: text }`. The real shared command
    // parser ships when the second adapter (Discord) needs it; for
    // now this lets PR 19 exercise both tool shapes through the
    // surface mapper.
    let (tool_name, args) = route_command(text);
    let tool_name: &str = tool_name;
    dispatch_and_render(
        adapter,
        principal,
        principal_for_post,
        tool_name.to_string(),
        args,
        chat_id,
        form_key.telegram_user_id,
    )
    .await
}

/// Dispatch a (tool, args) pair, then render the result back to
/// Telegram. Shared between the normal command path and the
/// post-form-completion path; the only difference is whether the
/// tool name comes from `route_command` (static) or from a
/// completed form's `submit_tool` (dynamic).
///
/// Owns the post-dispatch logic that PR 32 adds: if the dispatch
/// result is a form-only Surface, intercept and install the
/// per-chat numbered-prompts state instead of routing through the
/// surface mapper.
async fn dispatch_and_render(
    adapter: Arc<TelegramAdapter>,
    principal: Principal,
    principal_for_post: Principal,
    tool_name: String,
    args: Value,
    chat_id: i64,
    telegram_user_id: i64,
) -> Response {
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => {
            // PR 32: form-only Surfaces install per-chat state
            // and prompt field 1, INSTEAD of rendering as text.
            // Mixed surfaces (form + other components) still
            // route through `render_dispatch_result` where Form
            // defers as the v0.2 rough edge.
            if let Some(form_only) = surface_mapper::try_extract_form_only(&dispatch.result) {
                return install_form_and_prompt(
                    &adapter,
                    &principal_for_post,
                    &tool_name,
                    chat_id,
                    telegram_user_id,
                    form_only,
                )
                .await;
            }
            match render_dispatch_result(&dispatch.result, &adapter.correlation_key) {
                Ok(rendered) => {
                    if rendered.deferred_buttons > 0 {
                        tracing::warn!(
                            tool = %tool_name,
                            deferred_buttons = rendered.deferred_buttons,
                            "telegram surface mapper: button components deferred until correlation-token PR",
                        );
                    }
                    if rendered.deferred_selections > 0 {
                        tracing::warn!(
                            tool = %tool_name,
                            deferred_selections = rendered.deferred_selections,
                            "telegram surface mapper: Selection components deferred (empty options or token-cap overflow)",
                        );
                    }
                    if rendered.deferred_dashboards > 0 {
                        tracing::warn!(
                            tool = %tool_name,
                            deferred_dashboards = rendered.deferred_dashboards,
                            "telegram surface mapper: Dashboard components deferred until rasterizer wires in",
                        );
                    }
                    if rendered.truncated {
                        tracing::warn!(
                            tool = %tool_name,
                            cap_bytes = surface_mapper::TELEGRAM_TEXT_MAX_BYTES,
                            "telegram surface mapper: rendered text exceeded cap; truncated",
                        );
                    }
                    post_back(&adapter, &principal_for_post, &tool_name, chat_id, rendered).await;
                    StatusCode::OK.into_response()
                }
                Err(surface_mapper::RenderError::EmptyAfterRender) => {
                    tracing::warn!(
                        tool = %tool_name,
                        "telegram surface mapper: empty surface (no renderable components); skipping post-back",
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
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "telegram tool dispatch failed");
            telegram_status_for(&e).into_response()
        }
    }
}

/// Process the outcome of feeding a user message into an active
/// form. Returns the HTTP response the webhook should send back.
async fn handle_form_outcome(
    adapter: &Arc<TelegramAdapter>,
    principal_for_post: Principal,
    form_key: &form_state::FormKey,
    chat_id: i64,
    outcome: form_state::AdvanceOutcome,
) -> Response {
    match outcome {
        form_state::AdvanceOutcome::NeedMore => {
            // Send the next prompt. The peek goes back into the
            // store (the lock was released after `advance`); no
            // other handler can race us into the slot because the
            // (chat, sender) key is unique to this request.
            let prompt = adapter
                .form_state
                .peek_prompt(form_key)
                .unwrap_or_else(|| "(form continues)".to_string());
            let reply = courier_reply(&prompt);
            post_back(adapter, &principal_for_post, "form_prompt", chat_id, reply).await;
            StatusCode::OK.into_response()
        }
        form_state::AdvanceOutcome::Reprompt { reason } => {
            // Coercion / required-empty failure: tell the user
            // what went wrong and ask the SAME field again. The
            // state machine did NOT advance, so peek returns the
            // same prompt.
            let prompt = adapter
                .form_state
                .peek_prompt(form_key)
                .unwrap_or_else(|| "(form continues)".to_string());
            let body = format!("{}\n\n{}", reason, prompt);
            let reply = courier_reply(&body);
            post_back(
                adapter,
                &principal_for_post,
                "form_reprompt",
                chat_id,
                reply,
            )
            .await;
            StatusCode::OK.into_response()
        }
        form_state::AdvanceOutcome::Complete { submit_tool, args } => {
            // All fields collected. Dispatch the submit tool with
            // the assembled args; render the result through the
            // normal path. Principal is rebuilt from the same
            // claims that installed the form — the form-state
            // store doesn't carry it (G-8: state is the minimal
            // thing needed to continue, not a snapshot of the
            // boot-time identity).
            //
            // PR 37 Finding 5: we look up `sender_table` by the
            // platform-asserted `telegram_user_id` from the form
            // key, NOT by scanning for a matching `sub`. The
            // platform id is the same key the inbound handler
            // already uses, so this lookup is unambiguous. (The
            // old code scanned for the first sub-match, which is
            // wrong when two ids share a sub.) If the table
            // changed between install and complete the new claims
            // win — the table is the source of truth.
            let sender_key = form_key.telegram_user_id.to_string();
            let Some(claims) = adapter.sender_table.get(&sender_key) else {
                // Sender disappeared mid-flight (table reload).
                // Drop the submission with a rejection audit + a
                // user-facing notice. No dispatch happens.
                record_rejection(
                    adapter,
                    &principal_for_post.sub,
                    &principal_for_post.tenant,
                    TritonError::Auth(format!(
                        "telegram user id `{}` no longer in sender_table; form submission dropped",
                        sender_key
                    )),
                );
                let reply =
                    courier_reply("Your session is no longer authorised; form submission dropped.");
                post_back(adapter, &principal_for_post, "form_dropped", chat_id, reply).await;
                return StatusCode::OK.into_response();
            };
            let principal = Principal {
                sub: claims.sub.clone(),
                scopes: claims.scopes.clone(),
                tenant: claims.tenant.clone(),
                raw_token: String::new(),
                trace_id: uuid::Uuid::new_v4().to_string(),
            };
            let principal_for_post = principal.clone();
            dispatch_and_render(
                adapter.clone(),
                principal,
                principal_for_post,
                submit_tool,
                args,
                chat_id,
                form_key.telegram_user_id,
            )
            .await
        }
    }
}

/// Install a per-chat numbered-prompts form and send the first
/// prompt. Called after a dispatch returns a form-only Surface.
async fn install_form_and_prompt(
    adapter: &Arc<TelegramAdapter>,
    principal_for_post: &Principal,
    dispatching_tool: &str,
    chat_id: i64,
    telegram_user_id: i64,
    form: surface_mapper::FormOnly,
) -> Response {
    let key = form_state::FormKey {
        chat_id,
        telegram_user_id,
    };
    let title = form.title.clone();
    let outcome = adapter.form_state.install(
        key.clone(),
        form.submit_tool,
        form.fields,
        principal_for_post.sub.clone(),
        principal_for_post.tenant.clone(),
    );
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            // Tool shape bug (no fields / too many / dup). Don't
            // install; reply with a polite refusal and audit as
            // a courier-post (no dispatch level, since the
            // dispatcher already ran successfully — the failure
            // is downstream of it). The operator sees the bug via
            // tracing::warn so they can fix the tool.
            tracing::warn!(tool = %dispatching_tool, error = %e, "telegram form install refused");
            let reply = courier_reply(&format!(
                "Sorry — the form returned by `{dispatching_tool}` is malformed and can't be rendered."
            ));
            post_back(
                adapter,
                principal_for_post,
                dispatching_tool,
                chat_id,
                reply,
            )
            .await;
            return StatusCode::OK.into_response();
        }
    };
    // Per-tenant LRU eviction: audit the DROPPED form's principal,
    // not the installer's. PR 37 Finding 6 — the operator needs to
    // see WHOSE state was lost, otherwise an attacker installing a
    // form right after a victim could mask the victim's loss with
    // their own (attacker's) sub.
    if let form_state::InstallOutcome::InstalledEvicted {
        evicted,
        evicted_sub,
        evicted_tenant,
    } = &outcome
    {
        record_rejection(
            adapter,
            evicted_sub,
            evicted_tenant,
            TritonError::Validation(format!(
                "form state per-tenant cap reached for tenant `{}`; evicted oldest form for (chat_id={}, telegram_user_id={}) on adapter `{}`",
                evicted_tenant, evicted.chat_id, evicted.telegram_user_id, adapter.name
            )),
        );
    }

    let prompt = adapter
        .form_state
        .peek_prompt(&key)
        .unwrap_or_else(|| "(form ready)".to_string());
    let body = format!("{title}\n\n{prompt}");
    let reply = courier_reply(&body);
    post_back(
        adapter,
        principal_for_post,
        dispatching_tool,
        chat_id,
        reply,
    )
    .await;
    StatusCode::OK.into_response()
}

/// Build a `RenderedMessage` for a plain-text courier reply. Used
/// by the form-state path (prompts, cancellation, errors) where
/// the message body is adapter-generated, not derived from a
/// tool's Surface.
fn courier_reply(text: &str) -> RenderedMessage {
    RenderedMessage {
        text: text.to_string(),
        parse_mode: None,
        reply_markup: None,
        deferred_buttons: 0,
        deferred_selections: 0,
        deferred_dashboards: 0,
        dashboard: None,
        truncated: false,
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

    // PR 28: per-tenant fair-share. Same shape as the message
    // path above — bucket keyed by verified tenant id.
    if let Err(retry_after) = adapter.per_tenant_limit.try_take(&claims.tenant) {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::RateLimited(format!(
                "tenant `{}` rate limit hit on adapter `{}`; retry in {:.2}s",
                claims.tenant, adapter.name, retry_after
            )),
        );
        let secs = retry_after.ceil().max(1.0) as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "tenant rate limited",
        )
            .into_response();
    }

    let Some(token) = cq.data.as_deref().filter(|s| !s.is_empty()) else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("callback_query without data".into()),
        );
        return (StatusCode::BAD_REQUEST, "callback_query missing data").into_response();
    };

    // PR 23 replay protection (architecture.md §8.7 lift).
    //
    // FAIL-CLOSED: Telegram's `callback_query.message.date` is the
    // Unix second the BOT sent the original message carrying the
    // button. The inbound webhook is secret_token-authenticated,
    // so `message.date` is platform-asserted and trusted. But
    // ABSENCE of a date is NOT a green light: a hostile platform
    // (or future Telegram payload shape we don't yet model) could
    // omit `message`/`date` to bypass the freshness gate. Codex
    // PR 23 review flagged this — we now reject every callback
    // whose freshness anchor is missing or zero.
    let message_date = cq.message.as_ref().map(|m| m.date).unwrap_or(0);
    if message_date == 0 {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Auth("callback_query missing message.date (freshness anchor)".into()),
        );
        return (StatusCode::UNAUTHORIZED, "missing message.date").into_response();
    }
    let now = unix_now_secs();
    let ttl = CALLBACK_TTL_SECS as u64;
    if message_date < now.saturating_sub(ttl) {
        let age = now.saturating_sub(message_date);
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Auth(format!(
                "stale callback: message age {age}s exceeds TTL {CALLBACK_TTL_SECS}s"
            )),
        );
        return (StatusCode::UNAUTHORIZED, "stale callback").into_response();
    }
    if message_date > now.saturating_add(CALLBACK_FUTURE_SKEW_SECS as u64) {
        // Future-dated beyond the small allowed skew: reject so a
        // platform that pre-stamps a button far in the future
        // can't extend its TTL window (Codex PR 23 concern).
        let skew = message_date.saturating_sub(now);
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Auth(format!(
                "future-dated callback: message {skew}s ahead of now"
            )),
        );
        return (StatusCode::UNAUTHORIZED, "future-dated callback").into_response();
    }

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
            // `demo_panel` reference tool emits a Surface covering
            // every component variant (Text + Narration + Selection
            // + Form + Dashboard + Button). PR 26's integration
            // test drives this through `/demo`. Gated on the same
            // `dev-token` feature as the tool itself so production
            // builds don't reserve the route for an unregistered
            // tool (Codex PR 25 review pattern).
            #[cfg(feature = "dev-token")]
            "demo" => return ("demo_panel", json!({})),
            // PR 32 (numbered_prompts): the `form_only_demo_multi`
            // tool emits a form-only Surface with one String, one
            // Integer, and one optional Boolean field. The
            // integration test drives the per-chat state machine
            // through this command. Same `dev-token` gate as the
            // tool itself.
            #[cfg(feature = "dev-token")]
            "form_only_demo_multi" => return ("form_only_demo_multi", json!({})),
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
        deferred_selections: 0,
        deferred_dashboards: 0,
        truncated: false,
        dashboard: None,
    })
}

async fn post_back(
    adapter: &TelegramAdapter,
    principal: &Principal,
    tool_name: &str,
    chat_id: i64,
    mut msg: RenderedMessage,
) {
    // PR 36: surface carries a Dashboard → call the rasterizer
    // (architecture.md §8.7 / FR-A-11). On success we dispatch
    // `sendPhoto` with the PNG + caption; on failure we strip the
    // dashboard, append a deferred-text placeholder, and fall back
    // to `sendMessage` so the user still gets SOMETHING. The
    // rasterizer call itself emits its own `phase: post,
    // result: rasterizer_call` audit line so the network hop is
    // visible to operators independent of the final post outcome.
    if let Some(dash) = msg.dashboard.take() {
        if let Some(rasterizer) = adapter.rasterizer.as_ref() {
            match rasterize_dashboard(adapter, principal, tool_name, rasterizer, &dash).await {
                Ok(png) => {
                    let fields = surface_mapper::build_send_photo_fields(chat_id, &msg);
                    let start = std::time::Instant::now();
                    let outcome = adapter
                        .courier
                        .send_photo(&adapter.bot_token, fields, png)
                        .await;
                    let latency_ms = start.elapsed().as_millis() as u64;
                    record_post_outcome(adapter, tool_name, principal, latency_ms, outcome);
                    return;
                }
                Err(_) => {
                    // Rasterizer failed — fall through to the
                    // sendMessage fallback. Append a one-line
                    // placeholder so the user sees that a
                    // dashboard was offered but couldn't render
                    // (operators see the failure in audit + logs).
                    // Title doesn't leak tile content, only the
                    // dashboard's name.
                    let placeholder = format!(
                        "<i>dashboard '{title}' unavailable (rasterizer failed)</i>",
                        title = html_escape_caption(&dash.title)
                    );
                    if msg.text.is_empty() {
                        msg.text = placeholder;
                    } else {
                        msg.text.push_str("\n\n");
                        msg.text.push_str(&placeholder);
                    }
                    if msg.parse_mode.is_none() {
                        msg.parse_mode = Some("HTML");
                    }
                }
            }
        } else {
            // No rasterizer configured — same deferred-text
            // shape as the pre-PR-36 path.
            let placeholder = format!(
                "<i>dashboard '{title}' deferred — rasterizer not configured</i>",
                title = html_escape_caption(&dash.title)
            );
            if msg.text.is_empty() {
                msg.text = placeholder;
            } else {
                msg.text.push_str("\n\n");
                msg.text.push_str(&placeholder);
            }
            if msg.parse_mode.is_none() {
                msg.parse_mode = Some("HTML");
            }
            tracing::warn!(
                tool = tool_name,
                dashboard_title = %dash.title,
                "telegram adapter: no rasterizer configured; dashboard deferred"
            );
        }
    }

    let body = surface_mapper::build_send_message_body(chat_id, &msg);
    let start = std::time::Instant::now();
    let outcome = adapter
        .courier
        .send_message_body(&adapter.bot_token, &body)
        .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    record_post_outcome(adapter, tool_name, principal, latency_ms, outcome);
}

fn record_post_outcome(
    adapter: &TelegramAdapter,
    tool_name: &str,
    principal: &Principal,
    latency_ms: u64,
    outcome: Result<SendOutcome, CourierError>,
) {
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
            let m = e.message();
            tracing::warn!(
                courier_label = label.as_str(),
                "telegram courier failed: {m}"
            );
            let provider = TritonError::Provider(m);
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

/// Drive the rasterizer call and audit the result. Emits the
/// special `phase: post, result: rasterizer_call` audit line on
/// success and `result: error:provider, status_label:
/// rasterizer_failed` on failure (per architecture §8.7's network
/// dependency audit shape).
async fn rasterize_dashboard(
    adapter: &TelegramAdapter,
    principal: &Principal,
    tool_name: &str,
    rasterizer: &RasterizerClient,
    dash: &surface_mapper::RasterDashboard,
) -> Result<Vec<u8>, RasterizerError> {
    // Logging discipline (spec §Constraints): NEVER log full tile
    // contents at info level. Title + tile count only.
    tracing::info!(
        tool = tool_name,
        dashboard_title = %dash.title,
        tile_count = dash.tiles.len(),
        "telegram adapter: calling rasterizer for dashboard"
    );
    let req = DashboardRequest {
        title: dash.title.clone(),
        tiles: dash.tiles.clone(),
    };
    let start = std::time::Instant::now();
    let result = rasterizer.render(&req).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => {
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Ok((200, "rasterizer_call")),
            );
        }
        Err(e) => {
            tracing::warn!(
                tool = tool_name,
                error = %e,
                latency_ms,
                "telegram adapter: rasterizer call failed"
            );
            let provider = TritonError::Provider(format!("rasterizer: {e}"));
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, 0, "rasterizer_failed")),
            );
        }
    }
    result
}

/// Minimal HTML escape for caption text — local rather than
/// reaching into `surface_mapper::html_escape` (which is
/// pub(crate)). The fallback placeholder is the only caller; the
/// rest of the captioning lives in the surface mapper itself.
fn html_escape_caption(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
        TritonError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
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
