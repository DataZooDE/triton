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
//! Deferred to follow-up PRs: interactive buttons/lists/templates
//! (PR-T3), inbound button-tap decode (PR-T4), `upstream` identity mode,
//! delivery-receipt status callbacks (PR-T6).

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
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
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
    /// Twilio Auth Token — doubles as the HMAC-SHA1 signing key (inbound)
    /// and the HTTP Basic password (outbound).
    auth_token: String,
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

        Ok(Self {
            name: name.to_string(),
            auth_token,
            account_sid,
            public_url,
            from,
            sender_table,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            courier,
            rate_limit,
            per_tenant_limit,
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
    let text = params.get("Body").copied().unwrap_or("");
    // Statuses / delivery receipts land on the separate status-callback
    // route (PR-T6); an inbound message webhook with no `Body` (e.g. a
    // media-only message we don't model yet) is silently 200'd so Twilio
    // doesn't retry.
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
        .send_message(&adapter.account_sid, &adapter.auth_token, &params)
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

    /// Deliver an agent-initiated send. PR-T2 scope: unconditional
    /// free-form text — the Meta 24-hour service-window + template
    /// requirement (which Twilio's WhatsApp BSP path also carries) is
    /// deferred to PR-T3 alongside the rest of the template machinery.
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
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
