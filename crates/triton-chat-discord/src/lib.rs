//! v0.2 PR 22 — Discord chat-channel adapter.
//!
//! Discord's interaction model is synchronous: the platform POSTs
//! an Interaction to our webhook and we respond inline with a
//! `type: 4` channel-message (no separate outbound API call for
//! the immediate reply). This collapses Telegram's inbound +
//! outbound + audit-post into a single round-trip, but the
//! audit shape stays the same: `phase: dispatch` for the
//! tool invocation and `phase: post` for the channel-message we
//! returned in the HTTP response body.
//!
//! Scope of PR 22:
//!
//! * Inbound: Ed25519 signature verification on
//!   `X-Signature-Ed25519` + `X-Signature-Timestamp` headers,
//!   matching Discord's documented webhook validation.
//! * `type: 1` (PING) → `type: 1` (PONG). Discord uses this to
//!   probe the endpoint URL is alive.
//! * `type: 3` (MESSAGE_COMPONENT, i.e. button click): pull the
//!   correlation token from `data.custom_id`, verify via the
//!   shared `triton-correlation` crate, dispatch the recovered
//!   `(tool, args)`, return a `type: 4` UPDATE_MESSAGE with the
//!   rendered Surface.
//!
//! PR 29 adds `type: 2` (APPLICATION_COMMAND) — slash commands.
//! Operator still owns Discord-side command registration; this
//! adapter just dispatches the named command with its options
//! flattened into a JSON args map.
//!
//! PR 30 adds `type: 5` (MODAL_SUBMIT) and the inverse path:
//! tool surfaces containing exactly one `Component::Form`
//! become `type: 9` interaction responses (modal openers)
//! instead of channel messages. Modal correlation tokens use
//! the 100-byte Discord-native cap
//! ([`triton_correlation::DISCORD_MAX_CUSTOM_ID`]) rather than
//! Telegram's 64-byte `callback_data` budget.
//!
//! Reuses the Telegram surface mapper's chunk strategy but emits
//! Discord-native Markdown for narration (`*…*` for italics)
//! instead of HTML, and components v2 (`{type:1 ActionRow,
//! components:[{type:2 Button, …}]}`) for buttons.

mod surface_mapper;
pub use surface_mapper::RenderedInteraction;

/// Discord Gateway (persistent WebSocket) socket inbound — the
/// alternative to the Interactions webhook in this file.
pub mod gateway;
pub use gateway::DiscordGatewayAdapter;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::{Dispatcher, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_rasterizer::{Client as RasterizerClient, DashboardRequest, RasterizerError};
use triton_secrets::{ResolveError, SecretResolver};

pub const PROTOCOL: &str = "messenger:discord";
const HEADER_SIG: &str = "X-Signature-Ed25519";
const HEADER_TIMESTAMP: &str = "X-Signature-Timestamp";

/// Maximum Ed25519 timestamp skew (seconds). Discord recommends
/// rejecting requests older than ~5 minutes to prevent replay.
const MAX_TIMESTAMP_SKEW_SECS: u64 = 300;

/// PR 23: how stale a button click can be before we refuse it.
/// Calibrated from `interaction.message.timestamp` (when the bot
/// rendered the message), not the click timestamp. Matches
/// `MAX_TIMESTAMP_SKEW_SECS` so operators see one consistent
/// number across both checks.
const CALLBACK_TTL_SECS: u32 = 300;

/// PR 23: small allowance for a platform clock running ahead of
/// ours. Mirrors the Telegram adapter's value. 60 s covers normal
/// NTP drift while bounding any clock-skew-extends-TTL attack
/// (Codex PR 23 concern).
const CALLBACK_FUTURE_SKEW_SECS: u32 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

pub struct DiscordAdapter {
    name: String,
    verifying_key: VerifyingKey,
    correlation_key: Vec<u8>,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    /// PR 28: per-tenant rate limit (NFR-P-3 second tier).
    /// Consumed AFTER sender resolution.
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    /// PR 38: optional out-of-process dashboard rasterizer (FR-A-11).
    /// `None` falls back to the pre-PR-38 deferred-text path for
    /// `Component::Dashboard`. `Some(client)` ships rasterised
    /// PNGs as multipart interaction-response attachments.
    rasterizer: Option<RasterizerClient>,
}

impl DiscordAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        rasterizer: Option<RasterizerClient>,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::Discord {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::Ed25519 {
            return Err(BuildError::Unsupported(format!(
                "discord adapter requires `signature: ed25519`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "discord adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let pk_field = adapter
            .inbound
            .credentials
            .get("public_key")
            .ok_or(BuildError::MissingCredential("inbound.public_key"))?;
        let public_key_hex = resolver
            .resolve(pk_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.public_key", e))?;
        let pk_bytes = hex::decode(public_key_hex.trim())
            .map_err(|e| BuildError::Unsupported(format!("inbound.public_key not hex: {e}")))?;
        let pk_array: [u8; 32] = pk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| BuildError::Unsupported("inbound.public_key must be 32 bytes".into()))?;
        let verifying_key = VerifyingKey::from_bytes(&pk_array)
            .map_err(|e| BuildError::Unsupported(format!("inbound.public_key invalid: {e}")))?;

        // FR-L-6 / NFR-S-5: resolve every credential at boot, even
        // when the consuming PR doesn't use it yet. Outbound token
        // (REST API token for follow-up messages) lands when we
        // ship slash-command registration; we still resolve it now
        // so a misconfigured Vault ref fails closed.
        if let Some(field) = adapter.outbound.credentials.get("token") {
            resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.token", e))?;
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

        let correlation_key = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?
            .into_bytes();

        // PR 28: see triton-chat-telegram for the 10x headroom
        // rationale (adapter-wide is DoS-floor, per-tenant is
        // fair-share; same shape on both adapters).
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
            verifying_key,
            correlation_key,
            sender_table,
            dispatcher,
            rate_limit,
            per_tenant_limit,
            rasterizer,
        })
    }

    pub fn router(self: Arc<Self>) -> Router {
        let name = self.name.clone();
        let path = format!("/{name}/interactions");
        Router::new()
            .route(&path, post(handle_interaction))
            .with_state(self)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: discord`")]
    WrongKind,
    #[error("PR 22 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

async fn handle_interaction(
    State(adapter): State<Arc<DiscordAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Signature first (FR-I-8 / M-SIG-1). Verifies BEFORE we parse
    // the body so a malformed body from an unauthenticated source
    // never reaches serde or the dispatch path. Matches Telegram's
    // PR 13 verify-before-parse discipline.
    if let Err(reason) = verify_signature(&adapter, &headers, &body) {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth(format!("ed25519: {reason}")),
        );
        return (StatusCode::UNAUTHORIZED, "signature").into_response();
    }

    // PR 24: per-adapter rate limit (NFR-P-3). Same placement as
    // Telegram — after the signature check, before body parsing.
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "discord adapter `{}` rate limit hit; retry in {:.2}s",
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

    let interaction: DiscordInteraction = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed interaction body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed").into_response();
        }
    };

    match interaction.kind {
        1 => {
            // PING: Discord probes the endpoint with this; reply
            // with PONG so the bot registration validates the URL.
            (StatusCode::OK, axum::Json(json!({ "type": 1 }))).into_response()
        }
        2 => handle_application_command(&adapter, interaction).await,
        3 => handle_message_component(&adapter, interaction).await,
        5 => handle_modal_submit(&adapter, interaction).await,
        other => {
            // No remaining Discord interaction types currently in
            // scope. Codex PR 30 review fix: every refused inbound
            // MUST emit a rejection audit so a future Discord
            // protocol revision can't silently bypass the pivot.
            // Sender resolution has not happened (and may not be
            // safe to attempt on an unknown payload shape), so
            // sub/tenant are placeholders.
            tracing::warn!(
                interaction_type = other,
                "discord interaction type not supported",
            );
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("unsupported discord interaction type {other}")),
            );
            let body = json!({
                "type": 4,
                "data": {
                    "content": format!(
                        "_(Interaction type {other} not supported.)_"
                    ),
                    "flags": 64
                }
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
}

async fn handle_message_component(
    adapter: &Arc<DiscordAdapter>,
    interaction: DiscordInteraction,
) -> Response {
    // Sender resolution (FR-I-7). For DMs Discord puts the user
    // on `interaction.user`; for guild interactions on
    // `interaction.member.user`. Either path lands the same id in
    // the sender_table lookup.
    let user_id = interaction
        .user
        .as_ref()
        .or_else(|| interaction.member.as_ref().and_then(|m| m.user.as_ref()))
        .map(|u| u.id.clone())
        .unwrap_or_default();
    let Some(claims) = adapter.sender_table.get(&user_id) else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {user_id}")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

    // PR 28: per-tenant fair-share (NFR-P-3 second tier).
    // Discord adapter-wide bucket already consumed in
    // `handle_interaction`; here we add the per-tenant gate.
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

    let Some(data) = interaction.data else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("message_component without data".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing data").into_response();
    };
    let Some(token) = data.custom_id.as_deref().filter(|s| !s.is_empty()) else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("message_component without custom_id".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing custom_id").into_response();
    };

    // PR 23 replay protection. `interaction.message.timestamp` is
    // when Discord persisted the original message carrying the
    // button. The Ed25519 signature already authenticated the
    // inbound, so the timestamp is platform-asserted and trusted.
    //
    // FAIL-CLOSED: missing message, missing timestamp, or
    // unparseable timestamp are all rejected. Codex PR 23 review
    // caught the opt-in shape; a hostile platform (or future
    // Discord payload variant we don't yet model) could omit
    // `message` to bypass the freshness gate otherwise.
    let ts_iso = interaction
        .message
        .as_ref()
        .and_then(|m| m.timestamp.as_deref());
    let ts_iso = match ts_iso {
        Some(s) if !s.is_empty() => s,
        _ => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Auth(
                    "interaction missing message.timestamp (freshness anchor)".into(),
                ),
            );
            return (StatusCode::UNAUTHORIZED, "missing message.timestamp").into_response();
        }
    };
    let msg_secs = match chrono::DateTime::parse_from_rfc3339(ts_iso) {
        Ok(dt) => dt.timestamp(),
        Err(e) => {
            // Unparseable timestamp from an authenticated source:
            // fail closed rather than admit a click whose age we
            // can't verify.
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation(format!("unparseable message.timestamp: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "bad message timestamp").into_response();
        }
    };
    let now_secs = chrono::Utc::now().timestamp();
    if msg_secs > 0 && now_secs - msg_secs > CALLBACK_TTL_SECS as i64 {
        let age = now_secs - msg_secs;
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
    if msg_secs - now_secs > CALLBACK_FUTURE_SKEW_SECS as i64 {
        // Future-dated beyond the allowed skew (Codex PR 23
        // concern). Matches the Telegram adapter's policy.
        let skew = msg_secs - now_secs;
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

    let (tool_name, mut args) = match triton_correlation::decode(token, &adapter.correlation_key) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Auth(format!("custom_id token: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
        }
    };

    // PR 25 (Codex review blocker): a select-menu callback MUST
    // strictly match the shape the mapper emits:
    //   * `component_type == 3` (STRING_SELECT)
    //   * `data.values.len() == 1` (single-select only)
    //   * decoded args is exactly `{ <one_key>: null }`
    // Anything else is rejected so a forged or future-shaped
    // payload can't slip through. A Button callback (component_type
    // == 2) MUST NOT carry `values` either; a hostile platform
    // sending values on a button payload would otherwise mutate
    // the args before dispatch.
    let component_type = data.component_type;
    let has_values = !data.values.is_empty();
    match (component_type, has_values) {
        (Some(3), true) => {
            if data.values.len() != 1 {
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation(format!(
                        "select callback expects exactly 1 value; got {}",
                        data.values.len()
                    )),
                );
                return (StatusCode::BAD_REQUEST, "wrong values count").into_response();
            }
            let chosen = data.values.into_iter().next().unwrap();
            let Some(obj) = args.as_object_mut() else {
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation("select callback: args is not an object".into()),
                );
                return (StatusCode::BAD_REQUEST, "args not object").into_response();
            };
            if obj.len() != 1 {
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation(format!(
                        "select callback: args must have exactly 1 key; got {}",
                        obj.len()
                    )),
                );
                return (StatusCode::BAD_REQUEST, "args shape mismatch").into_response();
            }
            let (key, val) = obj.iter_mut().next().unwrap();
            if !val.is_null() {
                let key = key.clone();
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation(format!(
                        "select callback: args[{key}] must be null sentinel"
                    )),
                );
                return (StatusCode::BAD_REQUEST, "args slot not null").into_response();
            }
            *val = Value::String(chosen);
        }
        (Some(2), false) => {
            // Button callback, no values — the normal PR 21 path.
        }
        (Some(2), true) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation("button callback MUST NOT carry `data.values`".into()),
            );
            return (StatusCode::BAD_REQUEST, "button with values").into_response();
        }
        (Some(3), false) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation("select callback missing data.values".into()),
            );
            return (StatusCode::BAD_REQUEST, "select missing values").into_response();
        }
        (Some(other), _) => {
            // PR 25 ships Button (2) + StringSelect (3); other
            // component types (role/user/channel selects, text
            // inputs) are out of scope and refused.
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation(format!("unsupported component_type {other}")),
            );
            return (StatusCode::BAD_REQUEST, "unsupported component_type").into_response();
        }
        (None, _) => {
            // Discord always sends component_type on
            // MESSAGE_COMPONENT interactions; absence = forged.
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation("missing data.component_type".into()),
            );
            return (StatusCode::BAD_REQUEST, "missing component_type").into_response();
        }
    }

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let principal_for_post = principal.clone();

    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            // PR 30: form-only Surfaces become Discord modals
            // (type=9 interaction response). Check this BEFORE
            // the normal text+components path so a Form doesn't
            // get rendered as deferred text. Mixed surfaces (form
            // + other components) still fall through to the
            // existing path where Form defers.
            if let Some(form_result) =
                surface_mapper::try_render_form_modal(&dispatch.result, &adapter.correlation_key)
            {
                match form_result {
                    Ok(modal) => {
                        adapter.dispatcher.record_post(
                            &tool_name,
                            PROTOCOL,
                            &principal_for_post,
                            latency_ms,
                            Ok((200, "modal_opened")),
                        );
                        return (StatusCode::OK, axum::Json(modal)).into_response();
                    }
                    Err(e) => {
                        // Modal couldn't be built (too many fields,
                        // unsupported field kind, etc.). Log and
                        // fall through to the surface mapper so
                        // the user still sees something (deferred
                        // form rendering).
                        tracing::warn!(error = ?e, tool = tool_name, "form modal build failed; falling back to text render");
                    }
                }
            }
            match surface_mapper::try_render_surface(&dispatch.result, &adapter.correlation_key) {
                Some(Ok(rendered)) => {
                    build_response_with_rasterizer(
                        adapter,
                        &tool_name,
                        &principal_for_post,
                        rendered,
                        latency_ms,
                    )
                    .await
                }
                Some(Err(surface_mapper::RenderError::EmptyAfterRender)) => {
                    tracing::warn!(
                        tool = tool_name,
                        "discord surface mapper: empty surface; replying with dropped audit",
                    );
                    let provider =
                        TritonError::Provider("discord surface mapper: empty surface".into());
                    adapter.dispatcher.record_post(
                        &tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Err((&provider, 0, "dropped")),
                    );
                    // Still respond OK so Discord doesn't retry;
                    // ephemeral content tells the user nothing was
                    // rendered.
                    let body = json!({
                        "type": 4,
                        "data": { "content": "(no content)", "flags": 64 }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
                None => {
                    // Tool didn't emit a Surface; render the raw
                    // result as plain content. The result goes
                    // through Markdown escape + the same 2000-byte
                    // cap so non-A2UI tools can't bypass either
                    // guarantee (Codex PR 22 blocker 1).
                    let raw = bare_text(&dispatch.result);
                    let content = surface_mapper::clamp_plain_text(&raw);
                    adapter.dispatcher.record_post(
                        &tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Ok((200, "posted")),
                    );
                    let body = json!({
                        "type": 4,
                        "data": { "content": content }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "discord dispatch failed");
            // Codex PR 29 review: every Discord response after a
            // dispatch attempt must emit `phase: post` — including
            // the inline error ephemeral. 200 with the failure
            // body so Discord doesn't retry forever on a permanent
            // app-layer error (same retry-storm avoidance as the
            // Telegram courier).
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Err((&e, 0, "error_response")),
            );
            let body = json!({
                "type": 4,
                "data": { "content": format!("(error: {})", e.class()), "flags": 64 }
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
}

/// PR 29: APPLICATION_COMMAND (type 2) — slash command inbound.
///
/// Discord ships the slash-command name in `data.name` and the
/// typed-in arguments in `data.options[]`. The command itself must
/// have been pre-registered with Discord by the operator (out of
/// band of this adapter); at runtime we just dispatch whatever
/// name comes in against the manifest's tool table, with the same
/// sender resolution and per-tenant rate-limit gates as the
/// button-callback path. No `interaction.message.timestamp`
/// freshness check applies here — the Ed25519-signed envelope's
/// `X-Signature-Timestamp` (already verified by `verify_signature`)
/// IS the platform-asserted freshness anchor for slash commands;
/// the message-timestamp dance is type-3-only because component
/// callbacks correspond to a prior bot-rendered message.
async fn handle_application_command(
    adapter: &Arc<DiscordAdapter>,
    interaction: DiscordInteraction,
) -> Response {
    // Sender resolution (FR-I-7) first, same dual DM/guild
    // location as the message-component path.
    let user_id = interaction
        .user
        .as_ref()
        .or_else(|| interaction.member.as_ref().and_then(|m| m.user.as_ref()))
        .map(|u| u.id.clone())
        .unwrap_or_default();
    let Some(claims) = adapter.sender_table.get(&user_id) else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {user_id}")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

    // Per-tenant fair-share (NFR-P-3 second tier).
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

    let Some(data) = interaction.data else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("application_command without data".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing data").into_response();
    };
    let Some(tool_name) = data.name.as_deref().filter(|s| !s.is_empty()) else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("application_command without data.name".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing command name").into_response();
    };

    let args = match options_to_args(&data.options) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation(format!("slash command options: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "bad option").into_response();
        }
    };

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let principal_for_post = principal.clone();

    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            // PR 30: form-only Surfaces become Discord modals
            // (type=9 interaction response). Check this BEFORE
            // the normal text+components path so a Form doesn't
            // get rendered as deferred text. Mixed surfaces (form
            // + other components) still fall through to the
            // existing path where Form defers.
            if let Some(form_result) =
                surface_mapper::try_render_form_modal(&dispatch.result, &adapter.correlation_key)
            {
                match form_result {
                    Ok(modal) => {
                        adapter.dispatcher.record_post(
                            tool_name,
                            PROTOCOL,
                            &principal_for_post,
                            latency_ms,
                            Ok((200, "modal_opened")),
                        );
                        return (StatusCode::OK, axum::Json(modal)).into_response();
                    }
                    Err(e) => {
                        // Modal couldn't be built (too many fields,
                        // unsupported field kind, etc.). Log and
                        // fall through to the surface mapper so
                        // the user still sees something (deferred
                        // form rendering).
                        tracing::warn!(error = ?e, tool = tool_name, "form modal build failed; falling back to text render");
                    }
                }
            }
            match surface_mapper::try_render_surface(&dispatch.result, &adapter.correlation_key) {
                Some(Ok(rendered)) => {
                    build_response_with_rasterizer(
                        adapter,
                        tool_name,
                        &principal_for_post,
                        rendered,
                        latency_ms,
                    )
                    .await
                }
                Some(Err(surface_mapper::RenderError::EmptyAfterRender)) => {
                    let provider =
                        TritonError::Provider("discord surface mapper: empty surface".into());
                    adapter.dispatcher.record_post(
                        tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Err((&provider, 0, "dropped")),
                    );
                    let body = json!({
                        "type": 4,
                        "data": { "content": "(no content)", "flags": 64 }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
                None => {
                    let raw = bare_text(&dispatch.result);
                    let content = surface_mapper::clamp_plain_text(&raw);
                    adapter.dispatcher.record_post(
                        tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Ok((200, "posted")),
                    );
                    let body = json!({
                        "type": 4,
                        "data": { "content": content }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "discord slash-command dispatch failed");
            // Codex PR 29 review: every Discord response that
            // follows a dispatch attempt must audit as `phase: post`
            // — including the inline `(error: …)` ephemeral we send
            // back so Discord doesn't retry. Mirrors the success
            // path so operators can correlate dispatch + post on a
            // single interaction.
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Err((&e, 0, "error_response")),
            );
            let body = json!({
                "type": 4,
                "data": { "content": format!("(error: {})", e.class()), "flags": 64 }
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
}

/// PR 30: MODAL_SUBMIT (type 5) — the user has filled the modal
/// we opened from a Form-bearing surface and clicked submit.
///
/// Flow:
/// 1. Sender resolution + per-tenant rate limit (same as
///    button/slash paths).
/// 2. Decode `data.custom_id` (the correlation token we minted
///    when opening the modal) → recovers `(submit_tool,
///    args_skeleton)` where the skeleton's keys list the exact
///    field names the form committed to. A forged custom_id
///    can't substitute the tool or change the field set.
/// 3. Read `data.components[].components[]` — Discord's
///    Action-Row-wrapped TEXT_INPUTs. Each `custom_id` is the
///    field name, `value` is the user's text. Substitute into
///    the args skeleton; refuse any submission that adds fields
///    the token didn't authorise or omits required ones.
/// 4. Dispatch `submit_tool(args)`, render the result as a
///    type=4 channel message via the normal surface mapper.
async fn handle_modal_submit(
    adapter: &Arc<DiscordAdapter>,
    interaction: DiscordInteraction,
) -> Response {
    let user_id = interaction
        .user
        .as_ref()
        .or_else(|| interaction.member.as_ref().and_then(|m| m.user.as_ref()))
        .map(|u| u.id.clone())
        .unwrap_or_default();
    let Some(claims) = adapter.sender_table.get(&user_id) else {
        record_rejection(
            adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender {user_id}")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

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

    let Some(data) = interaction.data else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("modal_submit without data".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing data").into_response();
    };
    let Some(token) = data.custom_id.as_deref().filter(|s| !s.is_empty()) else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("modal_submit without custom_id".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing custom_id").into_response();
    };

    let (tool_name, mut args) = match triton_correlation::decode_with_cap(
        token,
        &adapter.correlation_key,
        triton_correlation::DISCORD_MAX_CUSTOM_ID,
    ) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Auth(format!("modal custom_id token: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
        }
    };

    // Substitute the user-supplied text inputs into the args
    // skeleton the token committed to. Refuse any submission that
    // doesn't match the expected shape exactly (Codex PR 25
    // pattern: forged payloads with extra/missing fields must
    // not slip past the cryptographic commit).
    let Some(skeleton) = args.as_object_mut() else {
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation("modal token args not object".into()),
        );
        return (StatusCode::BAD_REQUEST, "args shape mismatch").into_response();
    };

    let mut submitted: HashMap<String, String> = HashMap::new();
    for row in &data.components {
        for input in &row.components {
            // Codex PR 30 review: empty `custom_id` on a submitted
            // TEXT_INPUT is malformed (modal-opener guarantees
            // every TEXT_INPUT has a non-empty custom_id matching
            // a token-committed field name). Refuse instead of
            // silently skipping — silent-skip lets a forged
            // payload include controls whose values are accepted
            // by the user but then dropped server-side.
            if input.custom_id.is_empty() {
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation(
                        "modal submission has TEXT_INPUT with empty custom_id".into(),
                    ),
                );
                return (StatusCode::BAD_REQUEST, "empty field name").into_response();
            }
            if submitted
                .insert(input.custom_id.clone(), input.value.clone())
                .is_some()
            {
                record_rejection(
                    adapter,
                    &claims.sub,
                    &claims.tenant,
                    TritonError::Validation(format!(
                        "duplicate field `{}` in modal submission",
                        input.custom_id
                    )),
                );
                return (StatusCode::BAD_REQUEST, "duplicate field").into_response();
            }
        }
    }

    // Every skeleton key must appear in the submission; no extra
    // keys allowed. Discord normally enforces this client-side
    // (you can't submit a modal with surprise fields), but never
    // trust the platform — the token is what authorises this
    // dispatch and the token only commits to these field names.
    for (key, slot) in skeleton.iter_mut() {
        let Some(val) = submitted.remove(key) else {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation(format!("modal submission missing field `{key}`")),
            );
            return (StatusCode::BAD_REQUEST, "missing field").into_response();
        };
        if !slot.is_null() {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation(format!("modal token field `{key}` not a null slot")),
            );
            return (StatusCode::BAD_REQUEST, "args slot not null").into_response();
        }
        *slot = Value::String(val);
    }
    if !submitted.is_empty() {
        let extras: Vec<String> = submitted.keys().cloned().collect();
        record_rejection(
            adapter,
            &claims.sub,
            &claims.tenant,
            TritonError::Validation(format!("modal submission has unexpected fields {extras:?}")),
        );
        return (StatusCode::BAD_REQUEST, "unexpected fields").into_response();
    }

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let principal_for_post = principal.clone();

    let started = std::time::Instant::now();
    let result = adapter
        .dispatcher
        .invoke(&tool_name, args, principal, PROTOCOL)
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(dispatch) => {
            match surface_mapper::try_render_surface(&dispatch.result, &adapter.correlation_key) {
                Some(Ok(rendered)) => {
                    build_response_with_rasterizer(
                        adapter,
                        &tool_name,
                        &principal_for_post,
                        rendered,
                        latency_ms,
                    )
                    .await
                }
                Some(Err(surface_mapper::RenderError::EmptyAfterRender)) => {
                    let provider =
                        TritonError::Provider("discord surface mapper: empty surface".into());
                    adapter.dispatcher.record_post(
                        &tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Err((&provider, 0, "dropped")),
                    );
                    let body = json!({
                        "type": 4,
                        "data": { "content": "(no content)", "flags": 64 }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
                None => {
                    let raw = bare_text(&dispatch.result);
                    let content = surface_mapper::clamp_plain_text(&raw);
                    adapter.dispatcher.record_post(
                        &tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Ok((200, "posted")),
                    );
                    let body = json!({
                        "type": 4,
                        "data": { "content": content }
                    });
                    (StatusCode::OK, axum::Json(body)).into_response()
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "discord modal-submit dispatch failed");
            adapter.dispatcher.record_post(
                &tool_name,
                PROTOCOL,
                &principal_for_post,
                latency_ms,
                Err((&e, 0, "error_response")),
            );
            let body = json!({
                "type": 4,
                "data": { "content": format!("(error: {})", e.class()), "flags": 64 }
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
}

/// Flatten Discord's slash-command options array into a JSON args
/// map: `[{name,type,value}, ...] → { name: value, ... }`. The
/// option's `type` discriminator selects how to interpret `value`
/// — 3=string, 4=integer, 5=boolean. Other discriminators (incl.
/// 1/2 = sub-command/sub-command-group and 6-11 = entity refs)
/// return an error so we never silently drop or coerce arguments
/// a tool was expecting.
fn options_to_args(opts: &[DiscordCommandOption]) -> Result<Value, String> {
    let mut map = serde_json::Map::with_capacity(opts.len());
    for opt in opts {
        let v = match opt.kind {
            3 => {
                // STRING
                let Some(s) = opt.value.as_str() else {
                    return Err(format!(
                        "option `{}` (string) has non-string value",
                        opt.name
                    ));
                };
                Value::String(s.to_string())
            }
            4 => {
                // INTEGER. Discord ships int options as JSON
                // number; serde_json represents them as i64.
                let Some(n) = opt.value.as_i64() else {
                    return Err(format!(
                        "option `{}` (integer) has non-integer value",
                        opt.name
                    ));
                };
                Value::from(n)
            }
            5 => {
                // BOOLEAN
                let Some(b) = opt.value.as_bool() else {
                    return Err(format!(
                        "option `{}` (boolean) has non-bool value",
                        opt.name
                    ));
                };
                Value::Bool(b)
            }
            other => {
                return Err(format!(
                    "unsupported option type {other} for option `{}`",
                    opt.name
                ));
            }
        };
        if map.insert(opt.name.clone(), v).is_some() {
            return Err(format!("duplicate option name `{}`", opt.name));
        }
    }
    Ok(Value::Object(map))
}

/// Build the actual interaction response from a `RenderedInteraction`,
/// calling the rasterizer when the surface carries a Dashboard.
///
/// Returns:
///   * `Ok(Response)` — caller forwards verbatim to the HTTP client.
///   * `Err(rasterizer_error_string)` — caller emits a
///     `rasterizer_failed` post-audit and ships a fallback text
///     channel-message.
///
/// PR 38: Discord's interaction-response model is inline (no separate
/// outbound API call), so the multipart body IS the courier output.
/// On rasterizer failure we synthesise the same one-line placeholder
/// the Telegram adapter emits — operators get one consistent shape
/// across both platforms.
async fn build_response_with_rasterizer(
    adapter: &Arc<DiscordAdapter>,
    tool_name: &str,
    principal_for_post: &Principal,
    rendered: RenderedInteraction,
    latency_ms: u64,
) -> Response {
    let RenderedInteraction {
        content,
        components,
        dashboard,
        ..
    } = rendered;
    // No dashboard → plain JSON channel-message. Same shape as
    // pre-PR-38.
    let Some(dash) = dashboard else {
        let body = build_plain_response(&content, components.as_ref());
        adapter.dispatcher.record_post(
            tool_name,
            PROTOCOL,
            principal_for_post,
            latency_ms,
            Ok((200, "posted")),
        );
        return (StatusCode::OK, axum::Json(body)).into_response();
    };
    // Dashboard with no rasterizer configured → deferred-text
    // fallback. Operators see the gap via tracing; user still
    // gets the surrounding text.
    let Some(rasterizer) = adapter.rasterizer.as_ref() else {
        tracing::warn!(
            tool = tool_name,
            dashboard_title = %dash.title,
            "discord adapter: no rasterizer configured; dashboard deferred",
        );
        let placeholder = format!(
            "*(dashboard '{title}' deferred — rasterizer not configured)*",
            title = md_escape_caption(&dash.title)
        );
        let content_with_placeholder = if content.is_empty() {
            placeholder
        } else {
            format!("{content}\n\n{placeholder}")
        };
        let body = build_plain_response(&content_with_placeholder, components.as_ref());
        adapter.dispatcher.record_post(
            tool_name,
            PROTOCOL,
            principal_for_post,
            latency_ms,
            Ok((200, "posted")),
        );
        return (StatusCode::OK, axum::Json(body)).into_response();
    };
    // Rasterizer call. Audit the network hop separately from the
    // final post outcome so operators can see WHICH leg failed.
    match rasterize_dashboard(adapter, principal_for_post, tool_name, rasterizer, &dash).await {
        Ok(png) => {
            // Build the multipart interaction response: a
            // `payload_json` part carrying the channel-message
            // body + a `files[0]` part carrying the PNG bytes. The
            // `payload_json.attachments` array references the file
            // by index so Discord wires them up.
            let attachments = vec![json!({
                "id": 0,
                "filename": "dashboard.png",
                "description": dash.title,
            })];
            let mut data = serde_json::Map::new();
            if !content.is_empty() {
                data.insert("content".into(), Value::String(content.clone()));
            }
            if let Some(c) = components.as_ref() {
                data.insert("components".into(), c.clone());
            }
            data.insert("attachments".into(), Value::Array(attachments));
            let payload = json!({
                "type": 4,
                "data": Value::Object(data),
            });
            let (body, boundary) = build_multipart_body(&payload, &png);
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal_for_post,
                latency_ms,
                Ok((200, "posted")),
            );
            let content_type = format!("multipart/form-data; boundary={boundary}");
            (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Err(_e) => {
            // Rasterizer failed — fall back to text. The
            // rasterize_dashboard helper already emitted a
            // `rasterizer_failed` audit line; here we just emit
            // the user-facing channel-message + a regular
            // post-audit.
            let placeholder = format!(
                "*(dashboard '{title}' unavailable — rasterizer failed)*",
                title = md_escape_caption(&dash.title)
            );
            let content_with_placeholder = if content.is_empty() {
                placeholder
            } else {
                format!("{content}\n\n{placeholder}")
            };
            let body = build_plain_response(&content_with_placeholder, components.as_ref());
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                principal_for_post,
                latency_ms,
                Ok((200, "posted")),
            );
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
}

/// Build a plain type-4 channel-message body (no attachments).
fn build_plain_response(content: &str, components: Option<&Value>) -> Value {
    let mut data = serde_json::Map::new();
    data.insert("content".into(), Value::String(content.to_string()));
    if let Some(c) = components {
        data.insert("components".into(), c.clone());
    }
    json!({ "type": 4, "data": Value::Object(data) })
}

/// Build a `multipart/form-data` body carrying the `payload_json`
/// part + one `files[0]` PNG file part. Returns `(body, boundary)`
/// so the caller can stamp the matching `Content-Type` header.
///
/// Hand-rolled (no `reqwest::multipart::Form` here) because the
/// multipart body is consumed by axum's outgoing response, not by
/// `reqwest`'s request builder. The framing is the same RFC 2046
/// shape; just a different consumer.
fn build_multipart_body(payload_json: &Value, png: &[u8]) -> (Vec<u8>, String) {
    // Boundary: a fixed prefix + a uuid so we never collide with
    // a value that legitimately appears inside the PNG bytes.
    let boundary = format!("triton-discord-{}", uuid::Uuid::new_v4().simple());
    let mut buf: Vec<u8> = Vec::with_capacity(png.len() + 512);
    // Part 1: payload_json (Content-Type: application/json).
    buf.extend_from_slice(b"--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(b"Content-Disposition: form-data; name=\"payload_json\"\r\n");
    buf.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
    buf.extend_from_slice(
        serde_json::to_vec(payload_json)
            .unwrap_or_else(|_| b"{}".to_vec())
            .as_slice(),
    );
    buf.extend_from_slice(b"\r\n");
    // Part 2: files[0] (the PNG itself).
    buf.extend_from_slice(b"--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(
        b"Content-Disposition: form-data; name=\"files[0]\"; filename=\"dashboard.png\"\r\n",
    );
    buf.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    buf.extend_from_slice(png);
    buf.extend_from_slice(b"\r\n");
    // Closing boundary.
    buf.extend_from_slice(b"--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(b"--\r\n");
    (buf, boundary)
}

/// Drive the rasterizer call and audit the result. Emits the
/// special `phase: post, status_label: rasterizer_call` audit line
/// on success and `result: error:provider, status_label:
/// rasterizer_failed` on failure (per architecture §8.7's network
/// dependency audit shape — same wire format as the Telegram
/// adapter so operators don't have to learn a second schema).
async fn rasterize_dashboard(
    adapter: &Arc<DiscordAdapter>,
    principal: &Principal,
    tool_name: &str,
    rasterizer: &RasterizerClient,
    dash: &surface_mapper::RasterDashboard,
) -> Result<Vec<u8>, RasterizerError> {
    // Logging discipline: NEVER log raw tile contents at info level.
    // Title + tile count only (FR-AU-3 / spec Constraints).
    tracing::info!(
        tool = tool_name,
        dashboard_title = %dash.title,
        tile_count = dash.tiles.len(),
        "discord adapter: calling rasterizer for dashboard",
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
                "discord adapter: rasterizer call failed",
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

/// Minimal Markdown escape for caption text — local rather than
/// pulling the mapper's `md_escape` (which is private). The only
/// caller is the dashboard-fallback placeholder; everything else
/// goes through the surface mapper's escape pipeline.
fn md_escape_caption(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\\' | '*' | '_' | '~' | '`' | '>' | '|' | '#' | '-' | '+' | '.' | '!' | '[' | ']'
            | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn bare_text(v: &Value) -> String {
    if let Some(obj) = v.as_object()
        && obj.len() == 1
        && let Some(s) = obj.values().next().and_then(|v| v.as_str())
    {
        return s.to_string();
    }
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    serde_json::to_string(v).unwrap_or_else(|_| "<unrenderable>".to_string())
}

fn verify_signature(
    adapter: &DiscordAdapter,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(), &'static str> {
    let sig_hex = headers
        .get(HEADER_SIG)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-Signature-Ed25519")?;
    let timestamp = headers
        .get(HEADER_TIMESTAMP)
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-Signature-Timestamp")?;

    // Timestamp skew check (Codex PR 21's replay-protection note
    // is still relevant here): reject signed-but-stale requests.
    let ts_secs: u64 = timestamp.parse().map_err(|_| "timestamp parse")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let skew = now.saturating_sub(ts_secs).max(ts_secs.saturating_sub(now));
    if skew > MAX_TIMESTAMP_SKEW_SECS {
        return Err("timestamp skew too large");
    }

    let sig_bytes = hex::decode(sig_hex).map_err(|_| "signature not hex")?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "signature wrong length")?;
    let signature = Signature::from_bytes(&sig_array);

    let mut message = Vec::with_capacity(timestamp.len() + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(body);
    adapter
        .verifying_key
        .verify(&message, &signature)
        .map_err(|_| "ed25519 verify failed")?;
    Ok(())
}

fn record_rejection(adapter: &DiscordAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    user: Option<DiscordUser>,
    #[serde(default)]
    member: Option<DiscordMember>,
    /// Component interactions carry the originating message; its
    /// `timestamp` (ISO 8601) is when the bot rendered the button.
    /// PR 23 uses this as the freshness anchor for replay
    /// protection.
    #[serde(default)]
    message: Option<DiscordInteractionMessage>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionMessage {
    /// ISO 8601 UTC timestamp Discord assigns at message creation.
    /// We parse just enough to extract Unix seconds — no
    /// dependency on a full date crate.
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    custom_id: Option<String>,
    /// PR 29 (slash commands, interaction type 2): Discord puts
    /// the command name here. The command must have been
    /// pre-registered with Discord by the operator; this adapter
    /// just dispatches whatever name comes in (subject to the
    /// usual sender resolution + per-tenant rate-limit gates).
    #[serde(default)]
    name: Option<String>,
    /// PR 29: slash-command options. Discord ships them as
    /// `[{name, type, value}, ...]`; we flatten into a plain
    /// JSON map keyed by option name. PR 29 ships string +
    /// integer + boolean option types; richer types (channels,
    /// mentions, etc.) defer.
    #[serde(default)]
    options: Vec<DiscordCommandOption>,
    /// PR 25: string-select-menu callbacks carry the chosen option
    /// value(s) here. The mapper emits a select menu whose
    /// correlation token encodes `(tool, {args_key: null})`; at
    /// callback time we substitute `values[0]` for the null
    /// before dispatching. PR 25 supports single-select only
    /// (`min_values = max_values = 1`); the inbound handler
    /// rejects `values.len() != 1` so forged multi-select
    /// payloads can't bypass that contract.
    #[serde(default)]
    values: Vec<String>,
    /// Discord's component type discriminator: 2 = Button,
    /// 3 = STRING_SELECT, 5/6/7/8 = role/user/channel/mention
    /// selects, 4 = TextInput (modal). PR 25 only handles 2 and
    /// 3; everything else is refused at the inbound boundary so
    /// a forged payload can't dispatch through a code path the
    /// mapper would never emit.
    #[serde(default)]
    component_type: Option<u8>,
    /// PR 30: MODAL_SUBMIT (interaction type 5) carries the user's
    /// text inputs nested under `data.components[].components[]`.
    /// Each outer entry is an Action Row (type 1); each inner is
    /// a TEXT_INPUT (type 4) with a `custom_id` (the field name)
    /// and a `value` (what the user typed). Empty on non-modal
    /// interactions.
    #[serde(default)]
    components: Vec<DiscordModalRow>,
}

/// PR 30: Action Row inside a MODAL_SUBMIT payload. We don't care
/// about the row's `type` discriminator — Discord always sends
/// type=1 here — only the nested TEXT_INPUTs.
#[derive(Debug, Deserialize)]
struct DiscordModalRow {
    #[serde(default)]
    components: Vec<DiscordModalInput>,
}

/// PR 30: one TEXT_INPUT inside a modal-submit Action Row.
#[derive(Debug, Deserialize)]
struct DiscordModalInput {
    #[serde(default)]
    custom_id: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
}

/// One slash-command option: `{name, type, value}`. PR 29 handles
/// the three scalar option types (3=STRING, 4=INTEGER, 5=BOOLEAN);
/// nested sub-commands (option type 1/2) and entity references
/// (6/7/8/9 = USER/CHANNEL/ROLE/MENTIONABLE) defer until a tool
/// actually needs them — keeping the value space narrow makes the
/// "options → args" mapping a straight scalar copy with no
/// platform-side IDs leaking into the dispatcher.
#[derive(Debug, Deserialize)]
struct DiscordCommandOption {
    name: String,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    value: Value,
}

#[derive(Debug, Deserialize)]
struct DiscordMember {
    #[serde(default)]
    user: Option<DiscordUser>,
}
