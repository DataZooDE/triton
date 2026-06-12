//! WhatsApp Cloud API adapter (`kind: whatsapp_cloud`).
//!
//! The B2B-compliant WhatsApp transport — a verified Business number on
//! Meta's Graph API or an EU aggregator — distinct from the Baileys-
//! style Web socket bridge in [`bridge`] (`kind: whatsapp_web`).
//!
//! Mirrors the Telegram adapter's three-phase pipeline:
//!
//! 1. Inbound webhook (`POST /<adapter-name>/webhook`) — verifies
//!    the `X-Hub-Signature-256: sha256=<hex>` HMAC over the raw
//!    body using the configured app secret (FR-I-8 / M-SIG-1),
//!    parses Meta's nested `entry[*].changes[*].value.messages[]`
//!    envelope, resolves `messages[i].from` against the manifest's
//!    sender_table (FR-I-7), and dispatches a tool.
//! 2. Verify handshake (`GET /<adapter-name>/webhook`) — Meta's
//!    one-time subscription probe. We echo the supplied
//!    `hub.challenge` plain-text when the presented
//!    `hub.verify_token` matches the configured value; otherwise
//!    403. There's NO HMAC on the GET — it's the setup handshake.
//! 3. Outbound courier — POSTs the rendered Surface to
//!    `<api_base>/v18.0/{phone_number_id}/messages` with the
//!    bearer token in the `Authorization` header.
//!
//! Scope of PR 31:
//!   * Text + Narration only. Buttons / Selection / Form /
//!     Dashboard are counted into `deferred_*` and logged via
//!     `tracing::warn`, not rendered. Interactive primitives ship
//!     in PR 32.
//!   * Inbound message types other than `text` (e.g. statuses,
//!     reactions, media) are silently 200'd so Meta doesn't retry.
//!
//! Audit pivot stays in the dispatcher (ADR-6).

pub mod surface_mapper;
pub use surface_mapper::RenderedMessage;

/// WhatsApp Web bridge (persistent socket) inbound — the alternative
/// to the Cloud-API webhook in this file. Connects to a local
/// Baileys-style bridge daemon.
pub mod bridge;
pub use bridge::WhatsAppBridgeAdapter;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use triton_core::{
    Dispatcher, OutboundCourier, OutboundRequest, PostOutcome, Principal, TritonError,
};
use triton_manifest::{
    Adapter, AdapterKind, IdentityKind, SignatureScheme, TemplateCategory, TemplateDecl,
};
use triton_rasterizer::{Client as RasterizerClient, DashboardRequest, RasterizerError};
use triton_secrets::{ResolveError, SecretResolver};

pub const PROTOCOL: &str = "messenger:whatsapp";
const HEADER_SIGNATURE: &str = "X-Hub-Signature-256";

/// Per-WhatsApp-user claims resolved from the `sender_table`. The
/// table is keyed by the sender's `wa_id` (E.164 without leading
/// `+`) — that's the value Meta puts in `messages[i].from`.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// Configuration for the outbound courier half. Default base is
/// `https://graph.facebook.com`; tests override via
/// `TRITON_WHATSAPP_API_BASE` to point at the in-repo fake.
#[derive(Debug, Clone)]
pub struct CourierConfig {
    pub api_base: String,
    pub timeout: std::time::Duration,
}

impl Default for CourierConfig {
    fn default() -> Self {
        Self {
            api_base: "https://graph.facebook.com".to_string(),
            timeout: std::time::Duration::from_secs(10),
        }
    }
}

/// Built adapter; immutable after boot.
pub struct WhatsAppAdapter {
    name: String,
    /// HMAC-SHA256 app secret used to verify inbound webhook bodies.
    app_secret: Vec<u8>,
    /// Meta's webhook subscription verify token (configured in the
    /// app dashboard; sent back on the one-time GET handshake).
    verify_token: String,
    /// Bearer token for the outbound Graph API call.
    access_token: String,
    /// Phone-number ID embedded in the outbound URL path
    /// (`/v18.0/{phone_number_id}/messages`). Per-bot routing id.
    phone_number_id: String,
    sender_table: HashMap<String, SenderClaims>,
    /// Manifest `tool`: where plain inbound text dispatches (default
    /// `echo`). Commands (`/narrate` etc.) keep their special routes.
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    courier: CourierClient,
    /// NFR-P-3 first tier: adapter-wide DoS floor. See the matching
    /// fields on TelegramAdapter / DiscordAdapter for rationale.
    rate_limit: triton_core::ratelimit::TokenBucket,
    /// NFR-P-3 second tier: per-tenant fair-share gate.
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    /// PR 38: optional out-of-process dashboard rasterizer (FR-A-11).
    /// `None` falls back to the pre-PR-38 deferred-text path for
    /// `Component::Dashboard`. `Some(client)` uploads the rendered
    /// PNG to WhatsApp's media endpoint and sends an `image`
    /// message carrying the returned media_id.
    rasterizer: Option<RasterizerClient>,
    /// #94: Meta-approved message templates, keyed by category, from
    /// the manifest's `templates` map. Triton owns template selection;
    /// proactive sends outside the service window resolve a template
    /// here from the agent's category hint.
    templates: HashMap<TemplateCategory, TemplateDecl>,
    /// #94: in-memory 24-hour service-window tracker, keyed by `wa_id`,
    /// stamped on every inbound message. A proactive send inside the
    /// window may be free-form; outside it MUST use a template. In
    /// memory only — stateless across restarts (G-8); a cold start
    /// simply treats every recipient as window-closed until they
    /// message in again, which is the safe default.
    service_window: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// #94: per-adapter HMAC key signing interactive button/list `id`s
    /// so a future inbound `interactive`-reply handler can route a tap
    /// back to its `(tool, args)`.
    correlation_key: Vec<u8>,
}

/// WhatsApp's documented customer-service window: free-form messages are
/// allowed only within 24 h of the user's last inbound message.
const SERVICE_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

struct CourierClient {
    base: String,
    http: reqwest::Client,
}

impl WhatsAppAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        courier_config: CourierConfig,
        rasterizer: Option<RasterizerClient>,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::WhatsappCloud {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::Hmac256 {
            return Err(BuildError::Unsupported(format!(
                "whatsapp adapter requires `signature: hmac256`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "whatsapp adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let secret_field = adapter
            .inbound
            .credentials
            .get("secret")
            .ok_or(BuildError::MissingCredential("inbound.secret"))?;
        let app_secret = resolver
            .resolve(secret_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.secret", e))?
            .into_bytes();
        if app_secret.is_empty() {
            return Err(BuildError::Unsupported(
                "inbound.secret resolved to empty string".into(),
            ));
        }

        // Verify-token field name `verify_token` matches Meta's
        // dashboard label. Optional at the schema layer (the
        // manifest doesn't force it via check_required_credentials)
        // — adapters that skip the handshake can leave it unset;
        // PR 31's tests cover the present case.
        let verify_token = match adapter.inbound.credentials.get("verify_token") {
            Some(field) => resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("inbound.verify_token", e))?,
            None => String::new(),
        };

        let access_token = match adapter.outbound.credentials.get("token") {
            Some(field) => resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.token", e))?,
            None => return Err(BuildError::MissingCredential("outbound.token")),
        };
        let phone_number_id = match adapter.outbound.credentials.get("phone_number_id") {
            Some(field) => resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.phone_number_id", e))?,
            None => return Err(BuildError::MissingCredential("outbound.phone_number_id")),
        };

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

        // FR-L-6 / NFR-S-5: resolve at boot; a bad Vault ref must fail
        // closed at startup. #94 uses this key to sign the interactive
        // button/list `id`s the surface mapper emits.
        let correlation_key = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?
            .into_bytes();

        let courier = CourierClient::new(courier_config)?;
        // 10x headroom rationale matches Telegram PR 28.
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
        // #94: templates are copied from the manifest at boot — Triton
        // owns selection, the agent only hints the category.
        let templates: HashMap<TemplateCategory, TemplateDecl> =
            adapter.templates.clone().into_iter().collect();
        Ok(Self {
            name: name.to_string(),
            app_secret,
            verify_token,
            access_token,
            phone_number_id,
            sender_table,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            courier,
            rate_limit,
            per_tenant_limit,
            rasterizer,
            templates,
            service_window: std::sync::Mutex::new(HashMap::new()),
            correlation_key,
        })
    }

    /// Mount inbound webhook + verify-handshake at
    /// `/<adapter-name>/webhook`.
    pub fn router(self: Arc<Self>) -> Router {
        let path = format!("/{}/webhook", self.name);
        Router::new()
            .route(&path, get(handle_verify).post(handle_webhook))
            .with_state(self)
    }
}

/// Synthetic tool label for the audit `what`/`tool` field on an
/// agent-initiated proactive send — there's no inbound tool dispatch,
/// so this names the proactive surface instead.
const OUTBOUND_TOOL: &str = "outbound";

impl WhatsAppAdapter {
    /// Open (or refresh) the 24-h service window for `wa_id`.
    fn stamp_service_window(&self, wa_id: &str) {
        if let Ok(mut map) = self.service_window.lock() {
            map.insert(wa_id.to_string(), std::time::Instant::now());
        }
    }

    /// Whether `wa_id` is currently inside the 24-h service window.
    fn is_within_window(&self, wa_id: &str) -> bool {
        self.service_window
            .lock()
            .ok()
            .and_then(|m| m.get(wa_id).map(|t| t.elapsed() < SERVICE_WINDOW))
            .unwrap_or(false)
    }
}

#[async_trait]
impl OutboundCourier for WhatsAppAdapter {
    fn protocol(&self) -> &'static str {
        PROTOCOL
    }

    /// Deliver an agent-initiated send. With a `category` hint, Triton
    /// resolves the manifest template and posts a `type: template` body
    /// (the only thing Meta accepts outside the 24-h service window).
    /// Without a category, the recipient MUST be inside the window for a
    /// free-form text send; otherwise the send is refused. Audit stays
    /// in the dispatcher via `post_back` / `record_post` (ADR-6).
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        if let Some(category) = req.category.as_deref() {
            return self.deliver_template(req, principal, category).await;
        }
        if !self.is_within_window(&req.to) {
            return Err(TritonError::Validation(format!(
                "recipient {} is outside the 24-hour service window; a template `category` is required",
                req.to
            )));
        }
        let rendered = match render_dispatch_result(&req.result, &self.correlation_key) {
            Ok(r) => r,
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                return Err(TritonError::Validation(
                    "outbound surface rendered to nothing".into(),
                ));
            }
        };
        log_deferrals(OUTBOUND_TOOL, &rendered);
        post_back(self, principal, OUTBOUND_TOOL, &req.to, rendered).await;
        Ok(())
    }
}

impl WhatsAppAdapter {
    /// Resolve the agent's category hint to a manifest template and post
    /// a `type: template` body. Selection lives here (Triton owns the
    /// platform surface); the agent supplied only the category +
    /// ordered body variables.
    async fn deliver_template(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
        category: &str,
    ) -> Result<(), TritonError> {
        let parsed: TemplateCategory =
            serde_json::from_value(Value::String(category.to_string())).map_err(|_| {
                TritonError::Validation(format!(
                    "unknown template category `{category}` (expected utility|marketing|authentication)"
                ))
            })?;
        let Some(decl) = self.templates.get(&parsed) else {
            return Err(TritonError::Validation(format!(
                "no template configured for category `{category}` on adapter `{}`",
                self.name
            )));
        };
        let body = surface_mapper::build_template_body(
            &req.to,
            &decl.name,
            &decl.language,
            &req.variables,
        );
        let start = std::time::Instant::now();
        let outcome = self
            .courier
            .send_message(&self.access_token, &self.phone_number_id, &body)
            .await;
        let latency_ms = start.elapsed().as_millis() as u64;
        record_post_outcome(self, OUTBOUND_TOOL, principal, latency_ms, outcome);
        Ok(())
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

    /// POST `<base>/v18.0/{phone_number_id}/messages` with a bearer
    /// token. WhatsApp Cloud API returns 200 with a
    /// `{messages:[{id:"wamid...."}]}` envelope on success, or a
    /// non-2xx HTTP status carrying `{error: {message, code, ...}}`
    /// on failure. We classify retryable cases (429 + 5xx) as
    /// `retry` and other 4xx as `dropped`.
    async fn send_message(
        &self,
        access_token: &str,
        phone_number_id: &str,
        body: &Value,
    ) -> Result<SendOutcome, CourierError> {
        let url = format!("{}/v18.0/{}/messages", self.base, phone_number_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| CourierError::Transport(redact(&e.to_string(), access_token)))?;
        let http_status = resp.status().as_u16();
        if (200..300).contains(&http_status) {
            return Ok(SendOutcome {
                http_status,
                label: PostLabel::Posted,
            });
        }
        let label = if http_status == 429 || http_status >= 500 {
            PostLabel::Retry
        } else {
            PostLabel::Dropped
        };
        Err(CourierError::Application { http_status, label })
    }

    /// PR 38: upload a PNG to `<base>/v18.0/{phone_number_id}/media`.
    /// WhatsApp Cloud API's media endpoint accepts
    /// `multipart/form-data` carrying the file part + a
    /// `messaging_product=whatsapp` text part + a `type=image/png`
    /// text part, and returns `{id: "<media_id>"}` on success. The
    /// returned id is plugged into the subsequent image-message
    /// body's `image.id` field.
    async fn upload_media(
        &self,
        access_token: &str,
        phone_number_id: &str,
        png: Vec<u8>,
    ) -> Result<String, CourierError> {
        let url = format!("{}/v18.0/{}/media", self.base, phone_number_id);
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("dashboard.png")
            .mime_str("image/png")
            .map_err(|e| CourierError::Transport(redact(&e.to_string(), access_token)))?;
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .text("type", "image/png")
            .part("file", part);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(access_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| CourierError::Transport(redact(&e.to_string(), access_token)))?;
        let http_status = resp.status().as_u16();
        if !(200..300).contains(&http_status) {
            let label = if http_status == 429 || http_status >= 500 {
                PostLabel::Retry
            } else {
                PostLabel::Dropped
            };
            return Err(CourierError::Application { http_status, label });
        }
        // Decode the `{id: "..."}` envelope. A 200 with no id is a
        // malformed response — treat as Dropped so we don't try to
        // chain a send_message with an empty media_id.
        #[derive(Deserialize)]
        struct MediaResp {
            id: String,
        }
        let media: MediaResp = resp
            .json()
            .await
            .map_err(|e| CourierError::Transport(redact(&e.to_string(), access_token)))?;
        if media.id.is_empty() {
            return Err(CourierError::Application {
                http_status,
                label: PostLabel::Dropped,
            });
        }
        Ok(media.id)
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
    Posted,
    Retry,
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

    fn to_outcome(self) -> PostOutcome {
        match self {
            Self::Posted => PostOutcome::Posted,
            Self::Retry => PostOutcome::Retry,
            Self::Dropped => PostOutcome::Dropped,
        }
    }
}

#[derive(Debug)]
enum CourierError {
    Transport(String),
    Application { http_status: u16, label: PostLabel },
}

impl CourierError {
    fn label(&self) -> PostLabel {
        match self {
            Self::Transport(_) => PostLabel::Retry,
            Self::Application { label, .. } => *label,
        }
    }
    fn http_status(&self) -> u16 {
        match self {
            Self::Transport(_) => 0,
            Self::Application { http_status, .. } => *http_status,
        }
    }
    fn message(&self) -> String {
        match self {
            Self::Transport(m) => format!("whatsapp courier transport: {m}"),
            Self::Application { http_status, label } => format!(
                "whatsapp courier application: http_status={http_status}, label={}",
                label.as_str()
            ),
        }
    }
}

/// Strip every occurrence of the bearer token from a log/error
/// string so a stray `reqwest::Error::Display` (which sometimes
/// echoes the request URL or auth header) cannot leak it (FR-AU-3).
fn redact(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "<redacted>")
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: whatsapp_cloud`")]
    WrongKind,
    #[error("PR 31 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

// ---------- inbound: verify handshake ----------

#[derive(Debug, Deserialize)]
struct VerifyQuery {
    #[serde(rename = "hub.mode", default)]
    mode: String,
    #[serde(rename = "hub.verify_token", default)]
    verify_token: String,
    #[serde(rename = "hub.challenge", default)]
    challenge: String,
}

async fn handle_verify(
    State(adapter): State<Arc<WhatsAppAdapter>>,
    Query(q): Query<VerifyQuery>,
) -> Response {
    // Meta's docs: subscribe + matching token → echo challenge.
    // Anything else → 403. Constant-time compare on the token so
    // a verify_token guesser can't time-bisect.
    if q.mode == "subscribe"
        && !adapter.verify_token.is_empty()
        && constant_time_eq(q.verify_token.as_bytes(), adapter.verify_token.as_bytes())
    {
        return (StatusCode::OK, q.challenge).into_response();
    }
    record_rejection(
        &adapter,
        "-",
        "-",
        TritonError::Auth("whatsapp verify handshake mismatch".into()),
    );
    (StatusCode::FORBIDDEN, "forbidden").into_response()
}

// ---------- inbound: signed webhook ----------

#[derive(Debug, Deserialize)]
struct WebhookEnvelope {
    #[serde(default)]
    entry: Vec<EnvelopeEntry>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeEntry {
    #[serde(default)]
    changes: Vec<EnvelopeChange>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeChange {
    #[serde(default)]
    value: ChangeValue,
}

#[derive(Debug, Deserialize, Default)]
struct ChangeValue {
    #[serde(default)]
    messages: Vec<InboundMessage>,
}

#[derive(Debug, Deserialize)]
struct InboundMessage {
    #[serde(default)]
    from: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<MessageText>,
}

#[derive(Debug, Deserialize)]
struct MessageText {
    #[serde(default)]
    body: String,
}

async fn handle_webhook(
    State(adapter): State<Arc<WhatsAppAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Verify HMAC FIRST on raw bytes (FR-I-8). Body is `Bytes`,
    // not `Json`, so a malformed body from an unauthenticated
    // source never reaches serde. Mirrors the Telegram and
    // Discord verify-before-parse discipline.
    let presented = headers
        .get(HEADER_SIGNATURE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_hmac256(presented, &body, &adapter.app_secret) {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth("bad X-Hub-Signature-256".into()),
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // NFR-P-3 first tier: adapter-wide bucket.
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "whatsapp adapter `{}` rate limit hit; retry in {:.2}s",
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

    let envelope: WebhookEnvelope = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed webhook body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed").into_response();
        }
    };

    // Meta batches messages inside entry[].changes[].value.messages[].
    // Statuses/reactions/etc. live in the same envelope but on
    // different `field` keys; we only act on text-typed messages
    // and silently 200 everything else (FR-T category in the spec —
    // "accept and ignore" is the safe default for kinds we don't
    // model yet).
    for entry in envelope.entry {
        for change in entry.changes {
            for msg in change.value.messages {
                if msg.kind != "text" {
                    continue;
                }
                let Some(text) = msg.text.as_ref().map(|t| t.body.as_str()) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                if let Err(resp) = process_message(&adapter, &msg.from, text).await {
                    return resp;
                }
            }
        }
    }
    StatusCode::OK.into_response()
}

async fn process_message(
    adapter: &Arc<WhatsAppAdapter>,
    sender_key: &str,
    text: &str,
) -> Result<(), Response> {
    let Some(claims) = adapter.sender_table.get(sender_key) else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {sender_key}")),
        );
        return Err((StatusCode::UNAUTHORIZED, "unknown sender").into_response());
    };

    // #94: a verified inbound message opens this recipient's 24-hour
    // service window, so a later proactive send may go free-form.
    adapter.stamp_service_window(sender_key);

    // NFR-P-3 second tier: per-tenant fair-share, keyed by the
    // verified tenant id, not the platform `wa_id`.
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
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            "tenant rate limited",
        )
            .into_response());
    }

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };

    // Command parser mirrors Telegram's `route_command`: `/<tool>
    // <rest>` routes to `tool` with `{ subject: rest }`; anything
    // else falls through to the manifest-configured `tool` (default
    // `echo`) with the whole text as `{ message: text }`.
    let (tool_name, args) = route_command(text, &adapter.inbound_tool);
    let principal_for_post = principal.clone();
    let to = sender_key.to_string();
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => match render_dispatch_result(&dispatch.result, &adapter.correlation_key) {
            Ok(rendered) => {
                log_deferrals(&tool_name, &rendered);
                post_back(adapter, &principal_for_post, &tool_name, &to, rendered).await;
                Ok(())
            }
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                tracing::warn!(
                    tool = %tool_name,
                    "whatsapp surface mapper: empty surface (no renderable components); skipping post-back",
                );
                let provider =
                    TritonError::Provider("whatsapp surface mapper: empty surface".into());
                adapter.dispatcher.record_post(
                    &tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Err((&provider, 0, PostOutcome::Dropped, None)),
                );
                Ok(())
            }
        },
        Err(e) => {
            // Dispatcher already audited the failure. Meta retries
            // 5xx, so map permanent app-layer failures to 200 to
            // avoid retry storms — mirrors Telegram.
            tracing::warn!(error = %e, class = %e.class(), "whatsapp tool dispatch failed");
            // Swallow the error: the inbound webhook still acks 200
            // (Meta has nothing useful to retry for validation/auth
            // failures from the tool itself).
            Ok(())
        }
    }
}

fn log_deferrals(tool_name: &str, rendered: &RenderedMessage) {
    if rendered.deferred_buttons > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_buttons = rendered.deferred_buttons,
            "whatsapp surface mapper: Button components deferred (PR 32 wires interactive primitives)",
        );
    }
    if rendered.deferred_selections > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_selections = rendered.deferred_selections,
            "whatsapp surface mapper: Selection components deferred (PR 32)",
        );
    }
    if rendered.deferred_forms > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_forms = rendered.deferred_forms,
            "whatsapp surface mapper: Form components deferred (PR 32)",
        );
    }
    if rendered.deferred_dashboards > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_dashboards = rendered.deferred_dashboards,
            "whatsapp surface mapper: Dashboard components deferred until rasterizer wires in",
        );
    }
    if rendered.truncated {
        tracing::warn!(
            tool = tool_name,
            cap_bytes = surface_mapper::WHATSAPP_TEXT_MAX_BYTES,
            "whatsapp surface mapper: rendered text exceeded cap; truncated",
        );
    }
}

fn route_command(text: &str, default_tool: &str) -> (String, Value) {
    if let Some(rest) = text.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        match tool {
            "narrate" => return ("narrate".to_string(), json!({ "subject": subject })),
            // PR 38: dev-only command exercising the rasterizer
            // wiring. Same `dev-token` gate as the underlying
            // `demo_panel` tool so production builds don't reserve
            // a route for an unregistered tool.
            #[cfg(feature = "dev-token")]
            "demo" => return ("demo_panel".to_string(), json!({})),
            _ => {}
        }
    }
    (default_tool.to_string(), json!({ "message": text }))
}

fn render_dispatch_result(
    result: &serde_json::Value,
    correlation_key: &[u8],
) -> Result<RenderedMessage, surface_mapper::RenderError> {
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
        deferred_buttons: 0,
        deferred_selections: 0,
        deferred_forms: 0,
        deferred_dashboards: 0,
        truncated: false,
        dashboard: None,
        interactive: None,
    })
}

async fn post_back(
    adapter: &WhatsAppAdapter,
    principal: &Principal,
    tool_name: &str,
    to: &str,
    mut msg: RenderedMessage,
) {
    // PR 38: surface carries a Dashboard → call the rasterizer +
    // upload the PNG to WhatsApp's media endpoint + send an
    // `image` message. On rasterizer failure we fall back to the
    // text path with a placeholder so the user gets SOMETHING
    // (mirrors Telegram PR 36's fallback shape).
    if let Some(dash) = msg.dashboard.take() {
        if let Some(rasterizer) = adapter.rasterizer.as_ref() {
            match rasterize_dashboard(adapter, principal, tool_name, rasterizer, &dash).await {
                Ok(png) => {
                    // Two-step courier: upload media first, then
                    // send the image message referencing it. The
                    // surrounding Text/Narration (if any) becomes
                    // the image's caption.
                    let upload_start = std::time::Instant::now();
                    let media = adapter
                        .courier
                        .upload_media(&adapter.access_token, &adapter.phone_number_id, png)
                        .await;
                    let upload_latency = upload_start.elapsed().as_millis() as u64;
                    let media_id = match media {
                        Ok(id) => id,
                        Err(e) => {
                            let label = e.label();
                            let http_status = e.http_status();
                            let m = e.message();
                            tracing::warn!(
                                courier_label = label.as_str(),
                                "whatsapp media upload failed: {m}"
                            );
                            let provider = TritonError::Provider(m);
                            adapter.dispatcher.record_post(
                                tool_name,
                                PROTOCOL,
                                principal,
                                upload_latency,
                                Err((&provider, http_status, label.to_outcome(), None)),
                            );
                            return;
                        }
                    };
                    let caption: Option<&str> = if msg.text.is_empty() {
                        None
                    } else {
                        Some(msg.text.as_str())
                    };
                    let body = surface_mapper::build_image_message_body(to, &media_id, caption);
                    let send_start = std::time::Instant::now();
                    let outcome = adapter
                        .courier
                        .send_message(&adapter.access_token, &adapter.phone_number_id, &body)
                        .await;
                    let send_latency = send_start.elapsed().as_millis() as u64;
                    record_post_outcome(adapter, tool_name, principal, send_latency, outcome);
                    return;
                }
                Err(_) => {
                    // Rasterizer failed — fall through to text
                    // fallback. Append a one-line placeholder so
                    // the user knows a dashboard was offered.
                    // Tile content stays out of the placeholder
                    // (would silently violate `rasterised_png`).
                    let placeholder = format!(
                        "(dashboard '{title}' unavailable — rasterizer failed)",
                        title = dash.title
                    );
                    if msg.text.is_empty() {
                        msg.text = placeholder;
                    } else {
                        msg.text.push_str("\n\n");
                        msg.text.push_str(&placeholder);
                    }
                }
            }
        } else {
            // No rasterizer configured — same deferred-text shape
            // as the pre-PR-38 path.
            let placeholder = format!(
                "(dashboard '{title}' deferred — rasterizer not configured)",
                title = dash.title
            );
            if msg.text.is_empty() {
                msg.text = placeholder;
            } else {
                msg.text.push_str("\n\n");
                msg.text.push_str(&placeholder);
            }
            tracing::warn!(
                tool = tool_name,
                dashboard_title = %dash.title,
                "whatsapp adapter: no rasterizer configured; dashboard deferred"
            );
        }
    }

    // #94: an interactive surface (Buttons/Selection) ships as a
    // `type: interactive` message; otherwise plain text.
    let body = match &msg.interactive {
        Some(interactive) => surface_mapper::build_interactive_body(to, interactive),
        None => surface_mapper::build_messages_body(to, &msg),
    };
    let start = std::time::Instant::now();
    let outcome = adapter
        .courier
        .send_message(&adapter.access_token, &adapter.phone_number_id, &body)
        .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    record_post_outcome(adapter, tool_name, principal, latency_ms, outcome);
}

fn record_post_outcome(
    adapter: &WhatsAppAdapter,
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
                Ok((send.http_status, send.label.to_outcome(), None)),
            );
        }
        Err(e) => {
            let label = e.label();
            let http_status = e.http_status();
            let msg = e.message();
            tracing::warn!(
                courier_label = label.as_str(),
                "whatsapp courier failed: {msg}"
            );
            let provider = TritonError::Provider(msg);
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, http_status, label.to_outcome(), None)),
            );
        }
    }
}

/// Drive the rasterizer call and audit the result. Same audit shape
/// as Telegram PR 36 / Discord PR 38: `phase: post, status_label:
/// rasterizer_call` on success, `result: error:provider,
/// status_label: rasterizer_failed` on failure.
async fn rasterize_dashboard(
    adapter: &WhatsAppAdapter,
    principal: &Principal,
    tool_name: &str,
    rasterizer: &RasterizerClient,
    dash: &surface_mapper::RasterDashboard,
) -> Result<Vec<u8>, RasterizerError> {
    tracing::info!(
        tool = tool_name,
        dashboard_title = %dash.title,
        tile_count = dash.tiles.len(),
        "whatsapp adapter: calling rasterizer for dashboard",
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
                Ok((200, PostOutcome::Posted, Some("rasterizer_call"))),
            );
        }
        Err(e) => {
            tracing::warn!(
                tool = tool_name,
                error = %e,
                latency_ms,
                "whatsapp adapter: rasterizer call failed",
            );
            let provider = TritonError::Provider(format!("rasterizer: {e}"));
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((
                    &provider,
                    0,
                    PostOutcome::Dropped,
                    Some("rasterizer_failed"),
                )),
            );
        }
    }
    result
}

fn record_rejection(adapter: &WhatsAppAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}

/// Verify `X-Hub-Signature-256: sha256=<hex>` over `body` using
/// `app_secret`. Returns false on any malformed header, decode
/// error, or mismatch. Constant-time compare on the recovered
/// bytes (FR-I-8).
fn verify_hmac256(header: &str, body: &[u8], app_secret: &[u8]) -> bool {
    let Some(hex_part) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(presented) = hex::decode(hex_part) else {
        return false;
    };
    if presented.len() != 32 {
        return false;
    }
    let mut mac = match Hmac::<Sha256>::new_from_slice(app_secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    presented.ct_eq(computed.as_slice()).into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Match length AND content in constant time. Bounded scratch
    // sized to 256 — same upper bound the Telegram adapter uses for
    // its secret_token compare.
    const MAX: usize = 256;
    let mut a_buf = [0u8; MAX];
    let mut b_buf = [0u8; MAX];
    let a_n = a.len().min(MAX);
    let b_n = b.len().min(MAX);
    a_buf[..a_n].copy_from_slice(&a[..a_n]);
    b_buf[..b_n].copy_from_slice(&b[..b_n]);
    let content_eq: bool = a_buf.ct_eq(&b_buf).into();
    let length_eq = a.len() == b.len();
    content_eq & length_eq
}
