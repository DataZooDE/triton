//! Twilio-WhatsApp adapter (`kind: twilio_whatsapp`) — PR-T2 scope: text
//! and narration only, `sender_table` identity only. A parallel,
//! independently-selectable WhatsApp transport alongside the existing
//! `whatsapp_web` (Baileys socket) and `whatsapp_cloud` (direct Meta
//! Graph API) adapters; this one goes through Twilio's Business Solution
//! Provider path.
//!
//! Pipeline (mirrors the Telegram/WhatsApp Cloud three-phase shape):
//!
//! 1. Inbound webhook (`POST /<adapter-name>/webhook`) — Twilio POSTs
//!    `application/x-www-form-urlencoded` (NOT JSON). We parse the raw
//!    body into `(key, value)` pairs, verify `X-Twilio-Signature` over
//!    the adapter's configured public URL + those pairs (M-SIG-1: verify
//!    BEFORE acting on any of them — parsing form pairs is cheap and has
//!    no side effects, unlike deserialising into a typed struct), then
//!    dispatch on `From`/`Body`.
//! 2. Outbound courier — POSTs to Twilio's shared Messaging API
//!    (`triton_chat_twilio::courier::TwilioCourierClient`, also used by
//!    the planned RCS adapter).
//!
//! PR-T3 adds Content Template proactive sends (`category` +
//! `variables` on `OutboundRequest`, #94's existing mechanism, reused
//! verbatim): Twilio's WhatsApp channel requires a pre-approved Content
//! Template (referenced by `ContentSid`) for anything beyond a free-form
//! reply inside an active conversation — there is no ad-hoc
//! "build-buttons-at-send-time" the way Meta's direct Graph API allows
//! (`POST /Messages.json` only exposes `ContentSid` + `ContentVariables`
//! for rich content). So unlike WhatsApp Cloud, dynamically rendering a
//! surface's `Button`/`Selection` components into an interactive message
//! is NOT implementable without an operator pre-authoring a template per
//! distinct button set — a genuine platform constraint, not an
//! oversight. Runtime Button/Selection components stay deferred
//! (counted in `deferred_*`, same as PR-T2) until/unless a template
//! catalogue mechanism is designed for them.
//!
//! PR-T4 handles inbound button taps: since Twilio buttons aren't built
//! dynamically per message (see above), there's no signed correlation
//! token to decode the way Telegram's `callback_query` handler does — a
//! tap arrives as an ordinary inbound message carrying `ButtonPayload`
//! (the operator-authored postback string) alongside an often-empty
//! `Body`, so `handle_webhook` just prefers `ButtonPayload` over `Body`
//! as the routing text and dispatches through the same path.
//!
//! Deferred to follow-up PRs: `upstream` identity mode, delivery-receipt
//! status callbacks (PR-T6), 24-hour service-window enforcement
//! (WhatsApp Cloud's `is_within_window`/`stamp_service_window`
//! — Twilio's WhatsApp carries the same Meta-imposed window, but nothing
//! currently rejects a free-form send outside it; harmless in test/dev,
//! worth adding before relying on this in production).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use triton_core::{Dispatcher, OutboundCourier, OutboundRequest, Principal, TritonError};
use triton_manifest::{
    Adapter, AdapterKind, IdentityKind, SignatureScheme, TemplateCategory, TemplateDecl,
};
use triton_secrets::{ResolveError, SecretResolver};

use crate::courier::{CourierConfig, PostLabel, TwilioCourierClient};
use crate::signature;
use crate::surface_mapper::{self, RenderedMessage};

pub const PROTOCOL: &str = "messenger:twilio_whatsapp";
const HEADER_SIGNATURE: &str = "X-Twilio-Signature";
/// Synthetic tool label for the audit `what`/`tool` field on an
/// agent-initiated proactive send (mirrors every other courier's
/// `outbound`).
const OUTBOUND_TOOL: &str = "outbound";

/// Per-WhatsApp-sender claims resolved from the `sender_table`. Keyed by
/// the sender's E.164 number WITH the leading `+` (Twilio's `From` is
/// `whatsapp:+<E.164>`; we strip the `whatsapp:` prefix but keep the `+`
/// — unlike Meta's `wa_id`, which drops it).
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    pub tenant: String,
}

pub struct TwilioWhatsAppAdapter {
    name: String,
    /// `inbound.secret` — the HMAC-SHA1 key verifying `X-Twilio-Signature`.
    /// Operationally the same underlying Twilio Auth Token as
    /// `outbound_token` below, but resolved and used independently: a
    /// manifest that sets them to different values must fail closed at
    /// the courier (wrong Basic auth password) rather than silently
    /// accept whichever one happens to be configured (Codex review
    /// finding, #191 — `outbound.token` used to be required by the
    /// manifest's closed set but never actually consulted).
    auth_token: String,
    /// `outbound.token` — the HTTP Basic auth password for the Messages
    /// API courier.
    outbound_token: String,
    account_sid: String,
    /// The exact externally-visible URL Twilio POSTs to (e.g.
    /// `https://gateway.example.com/twilio-whatsapp/webhook`) — Twilio
    /// signs the URL it dialed, which the substrate's reverse proxy
    /// means axum's own view of the request URI cannot be trusted to
    /// reproduce (12-factor VII: Fabio sits in front, not the binary).
    public_url: String,
    /// Twilio WhatsApp sender, `whatsapp:+<E.164>` (manifest
    /// `outbound.from`).
    from: String,
    sender_table: HashMap<String, SenderClaims>,
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    courier: TwilioCourierClient,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    /// PR-T3: Content Templates (Twilio `ContentSid`s), keyed by the same
    /// Meta template category taxonomy WhatsApp Cloud's `templates` map
    /// uses (#94) — reused as-is since Meta's approval categories apply
    /// regardless of which BSP relays the send.
    templates: HashMap<TemplateCategory, TemplateDecl>,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: twilio_whatsapp`")]
    WrongKind,
    #[error("twilio_whatsapp PR-T2 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, #[source] ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

impl TwilioWhatsAppAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        courier_config: CourierConfig,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::TwilioWhatsapp {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::TwilioSignature {
            return Err(BuildError::Unsupported(format!(
                "twilio_whatsapp adapter requires `signature: twilio_signature`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "twilio_whatsapp adapter (PR-T2) supports only `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let auth_token = resolve(
            resolver,
            adapter.inbound.credentials.get("secret"),
            "inbound.secret",
        )
        .await?;
        let outbound_token = resolve(
            resolver,
            adapter.outbound.credentials.get("token"),
            "outbound.token",
        )
        .await?;
        let account_sid = resolve(
            resolver,
            adapter.outbound.credentials.get("account_sid"),
            "outbound.account_sid",
        )
        .await?;
        let public_url = resolve(
            resolver,
            adapter.inbound.credentials.get("public_url"),
            "inbound.public_url",
        )
        .await?;
        let from = resolve(
            resolver,
            adapter.outbound.credentials.get("from"),
            "outbound.from",
        )
        .await?;

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

        let courier = TwilioCourierClient::new(courier_config).map_err(BuildError::Unsupported)?;

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
        // PR-T3: templates are copied from the manifest at boot — Triton
        // owns category → ContentSid selection, the agent only hints the
        // category (mirrors WhatsApp Cloud's #94).
        let templates: HashMap<TemplateCategory, TemplateDecl> =
            adapter.templates.clone().into_iter().collect();

        Ok(Self {
            name: name.to_string(),
            auth_token,
            outbound_token,
            account_sid,
            public_url,
            from,
            sender_table,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            courier,
            rate_limit,
            per_tenant_limit,
            templates,
        })
    }

    /// Mount the inbound webhook at `/<adapter-name>/webhook`.
    pub fn router(self: Arc<Self>) -> Router {
        let path = format!("/{}/webhook", self.name);
        Router::new()
            .route(&path, post(handle_webhook))
            .with_state(self)
    }
}

async fn resolve(
    resolver: &dyn SecretResolver,
    field: Option<&triton_manifest::SecretField>,
    label: &'static str,
) -> Result<String, BuildError> {
    let field = field.ok_or(BuildError::MissingCredential(label))?;
    resolver
        .resolve(field)
        .await
        .map_err(|e| BuildError::Resolve(label, e))
}

async fn handle_webhook(
    State(adapter): State<Arc<TwilioWhatsAppAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Parse form pairs first (cheap, no side effects — NOT the same risk
    // as JSON-deserialising into a typed struct pre-auth) so we have
    // something to verify the signature over; nothing is ACTED on until
    // verification passes (M-SIG-1).
    let pairs: Vec<(String, String)> = match serde_urlencoded::from_bytes(&body) {
        Ok(p) => p,
        Err(_) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation("malformed x-www-form-urlencoded body".into()),
            );
            return (StatusCode::BAD_REQUEST, "malformed").into_response();
        }
    };
    let presented = headers
        .get(HEADER_SIGNATURE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    if !signature::verify(
        &adapter.public_url,
        &pair_refs,
        &adapter.auth_token,
        presented,
    ) {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth("bad X-Twilio-Signature".into()),
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "twilio_whatsapp adapter `{}` rate limit hit; retry in {:.2}s",
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

    let params: HashMap<&str, &str> = pair_refs.iter().copied().collect();
    let from = params.get("From").copied().unwrap_or("");
    // PR-T4: a tap on a Quick Reply / List button from a Content
    // Template arrives as an ordinary inbound message carrying
    // `ButtonPayload` (the operator-authored postback string baked into
    // the template at authoring time) alongside an often-empty `Body`.
    // There's no correlation token to decode here (unlike Telegram's
    // callback_query) — Twilio buttons aren't built dynamically per
    // message (see PR-T3), so `ButtonPayload` IS the routing input, same
    // as typed text. Prefer it when present; fall back to `Body`
    // otherwise so plain messages are unaffected.
    let text = match params.get("ButtonPayload").copied() {
        Some(payload) if !payload.is_empty() => payload,
        _ => params.get("Body").copied().unwrap_or(""),
    };
    // Statuses / delivery receipts land on the separate status-callback
    // route (PR-T6); an inbound message webhook with no `Body` or
    // `ButtonPayload` (e.g. a media-only message we don't model yet) is
    // silently 200'd so Twilio doesn't retry.
    if text.is_empty() {
        return StatusCode::OK.into_response();
    }
    let Some(sender_key) = from.strip_prefix("whatsapp:") else {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Validation("From is missing the `whatsapp:` prefix".into()),
        );
        return (StatusCode::BAD_REQUEST, "malformed From").into_response();
    };

    match process_message(&adapter, sender_key, text).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(resp) => resp,
    }
}

async fn process_message(
    adapter: &Arc<TwilioWhatsAppAdapter>,
    sender_key: &str,
    text: &str,
) -> Result<(), Response> {
    let claims = match adapter.sender_table.get(sender_key) {
        Some(c) => c.clone(),
        None => {
            record_rejection(
                adapter,
                "-",
                "-",
                TritonError::Auth(format!("unknown sender {sender_key}")),
            );
            return Err((StatusCode::UNAUTHORIZED, "unknown sender").into_response());
        }
    };

    if let Err(retry_after) = adapter.per_tenant_limit.try_take(&claims.tenant) {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::RateLimited(format!(
                "tenant `{}` rate limit hit on adapter `{}`; retry in {retry_after:.2}s",
                claims.tenant, adapter.name
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
        sub: claims.sub,
        scopes: claims.scopes,
        groups: claims.groups,
        tenant: claims.tenant,
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };

    let (tool_name, args) = route_command(text, &adapter.inbound_tool);
    let principal_for_post = principal.clone();
    let to = sender_key.to_string();
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => match render_dispatch_result(&dispatch.result) {
            Ok(rendered) => {
                log_deferrals(&tool_name, &rendered);
                post_back(adapter, &principal_for_post, &tool_name, &to, &rendered).await;
                Ok(())
            }
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                tracing::warn!(
                    tool = %tool_name,
                    "twilio_whatsapp surface mapper: empty surface; skipping post-back",
                );
                let provider =
                    TritonError::Provider("twilio_whatsapp surface mapper: empty surface".into());
                adapter.dispatcher.record_post(
                    &tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Err((&provider, 0, triton_core::audit::PostOutcome::Dropped, None)),
                );
                Ok(())
            }
        },
        Err(e) => {
            // Dispatcher already audited the failure. Twilio retries
            // 5xx, so map app-layer failures to 200 to avoid retry
            // storms (mirrors Telegram / WhatsApp Cloud).
            tracing::warn!(error = %e, class = %e.class(), "twilio_whatsapp tool dispatch failed");
            Ok(())
        }
    }
}

fn log_deferrals(tool_name: &str, rendered: &RenderedMessage) {
    if rendered.deferred_buttons > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_buttons = rendered.deferred_buttons,
            "twilio_whatsapp surface mapper: Button components deferred (PR-T3 wires interactive primitives)",
        );
    }
    if rendered.deferred_selections > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_selections = rendered.deferred_selections,
            "twilio_whatsapp surface mapper: Selection components deferred (PR-T3)",
        );
    }
    if rendered.deferred_forms > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_forms = rendered.deferred_forms,
            "twilio_whatsapp surface mapper: Form components deferred (PR-T3)",
        );
    }
    if rendered.deferred_dashboards > 0 {
        tracing::warn!(
            tool = tool_name,
            deferred_dashboards = rendered.deferred_dashboards,
            "twilio_whatsapp surface mapper: Dashboard components deferred (no rasterizer wiring yet)",
        );
    }
    if rendered.truncated {
        tracing::warn!(
            tool = tool_name,
            cap_bytes = surface_mapper::TWILIO_TEXT_MAX_BYTES,
            "twilio_whatsapp surface mapper: rendered text exceeded cap; truncated",
        );
    }
}

fn route_command(text: &str, default_tool: &str) -> (String, serde_json::Value) {
    if let Some(rest) = text.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        if tool == "narrate" {
            return (
                "narrate".to_string(),
                serde_json::json!({ "subject": subject }),
            );
        }
    }
    (
        default_tool.to_string(),
        serde_json::json!({ "message": text }),
    )
}

fn render_dispatch_result(
    result: &serde_json::Value,
) -> Result<RenderedMessage, surface_mapper::RenderError> {
    if let Some(r) = surface_mapper::try_render_surface(result) {
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
    })
}

async fn post_back(
    adapter: &TwilioWhatsAppAdapter,
    principal: &Principal,
    tool_name: &str,
    to: &str,
    msg: &RenderedMessage,
) {
    let to_param = format!("whatsapp:{to}");
    let params = [
        ("From", adapter.from.as_str()),
        ("To", to_param.as_str()),
        ("Body", msg.text.as_str()),
    ];
    let start = std::time::Instant::now();
    let outcome = adapter
        .courier
        .send_message(&adapter.account_sid, &adapter.outbound_token, &params)
        .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    record_post_outcome(adapter, tool_name, principal, latency_ms, outcome);
}

fn record_post_outcome(
    adapter: &TwilioWhatsAppAdapter,
    tool_name: &str,
    principal: &Principal,
    latency_ms: u64,
    outcome: Result<crate::courier::SendOutcome, crate::courier::CourierError>,
) {
    match outcome {
        Ok(send) => {
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Ok((send.http_status, to_post_outcome(send.label), None)),
            );
        }
        Err(e) => {
            let label = e.label();
            let http_status = e.http_status();
            let msg = e.message();
            tracing::warn!(
                courier_label = label.as_str(),
                "twilio courier failed: {msg}"
            );
            let provider = TritonError::Provider(msg);
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal,
                latency_ms,
                Err((&provider, http_status, to_post_outcome(label), None)),
            );
        }
    }
}

fn to_post_outcome(label: PostLabel) -> triton_core::audit::PostOutcome {
    match label {
        PostLabel::Posted => triton_core::audit::PostOutcome::Posted,
        PostLabel::Retry => triton_core::audit::PostOutcome::Retry,
        PostLabel::Dropped => triton_core::audit::PostOutcome::Dropped,
    }
}

fn record_rejection(adapter: &TwilioWhatsAppAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}

#[async_trait]
impl OutboundCourier for TwilioWhatsAppAdapter {
    fn protocol(&self) -> &'static str {
        PROTOCOL
    }

    /// #113 recipient/tenant binding: `to` MUST be a known recipient
    /// whose tenant matches the caller's.
    async fn authorize(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        match self.sender_table.get(&req.to) {
            Some(claims) if claims.tenant == principal.tenant => Ok(()),
            Some(_) => Err(TritonError::Forbidden(format!(
                "recipient {} is not in tenant `{}`",
                req.to, principal.tenant
            ))),
            None => Err(TritonError::Forbidden(format!(
                "recipient {} is not a known sender for this adapter",
                req.to
            ))),
        }
    }

    /// Deliver an agent-initiated send. With a `category` hint, Triton
    /// resolves the manifest Content Template and posts `ContentSid` +
    /// `ContentVariables` (PR-T3). Without one: unconditional free-form
    /// text — the Meta 24-hour service-window enforcement WhatsApp
    /// Cloud has is not yet ported here (see module docs).
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        if let Some(category) = req.category.as_deref() {
            return self.deliver_template(req, principal, category).await;
        }
        let rendered = match render_dispatch_result(&req.result) {
            Ok(r) => r,
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                return Err(TritonError::Validation(
                    "outbound surface rendered to nothing".into(),
                ));
            }
        };
        log_deferrals(OUTBOUND_TOOL, &rendered);
        post_back(self, principal, OUTBOUND_TOOL, &req.to, &rendered).await;
        Ok(())
    }
}

impl TwilioWhatsAppAdapter {
    /// Resolve the agent's category hint to a manifest Content Template
    /// and post `ContentSid` + `ContentVariables`. Selection lives here
    /// (Triton owns the platform surface); the agent supplied only the
    /// category + ordered body variables (#94, PR-T3).
    async fn deliver_template(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
        category: &str,
    ) -> Result<(), TritonError> {
        let parsed: triton_manifest::TemplateCategory = serde_json::from_value(
            serde_json::Value::String(category.to_string()),
        )
        .map_err(|_| {
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
        // Twilio's Content Template variables are keyed by position as
        // strings ("1", "2", ...), matching the `{{1}}`/`{{2}}`
        // placeholders the template itself defines.
        let variables: serde_json::Map<String, serde_json::Value> = req
            .variables
            .iter()
            .enumerate()
            .map(|(i, v)| ((i + 1).to_string(), serde_json::Value::String(v.clone())))
            .collect();
        let content_variables =
            serde_json::to_string(&serde_json::Value::Object(variables)).unwrap_or_default();
        let to_param = format!("whatsapp:{}", req.to);
        let params = [
            ("From", self.from.as_str()),
            ("To", to_param.as_str()),
            ("ContentSid", decl.name.as_str()),
            ("ContentVariables", content_variables.as_str()),
        ];
        let start = std::time::Instant::now();
        let outcome = self
            .courier
            .send_message(&self.account_sid, &self.outbound_token, &params)
            .await;
        let latency_ms = start.elapsed().as_millis() as u64;
        record_post_outcome(self, OUTBOUND_TOOL, principal, latency_ms, outcome);
        Ok(())
    }
}
