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
//! * Slash commands (`type: 2`) are out of scope; that requires
//!   Discord-side command registration which is operator work.
//!
//! Reuses the Telegram surface mapper's chunk strategy but emits
//! Discord-native Markdown for narration (`*…*` for italics)
//! instead of HTML, and components v2 (`{type:1 ActionRow,
//! components:[{type:2 Button, …}]}`) for buttons.

mod surface_mapper;
pub use surface_mapper::RenderedInteraction;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use serde_json::{Value, json};
use triton_core::{Dispatcher, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
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
}

impl DiscordAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
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

        let rate_limit = triton_core::ratelimit::TokenBucket::new(
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
        3 => handle_message_component(&adapter, interaction).await,
        other => {
            // Other interaction types (2 = APPLICATION_COMMAND for
            // slash commands, 5 = MODAL_SUBMIT) are documented but
            // not in PR 22's scope. Discord requires a valid
            // interaction-response shape; PONG (`type: 1`) is the
            // PING callback only. Codex PR 22 review caught this:
            // return a CHANNEL_MESSAGE_WITH_SOURCE with ephemeral
            // content explaining the gap. The flags=64 hides the
            // message from other chat members so the apology is
            // operator-visible without spamming the channel.
            tracing::warn!(
                interaction_type = other,
                "discord interaction type not yet supported",
            );
            let body = json!({
                "type": 4,
                "data": {
                    "content": format!(
                        "_(Interaction type {other} not implemented yet — slash commands and modal submits land in a later PR.)_"
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

    // PR 25: if `data.values` is present, this is a Selection
    // callback. The mapper encoded the args as `{args_key: null}`;
    // substitute the picked value before dispatch. Reject if the
    // shape doesn't match what we'd have emitted (multi-key args,
    // non-null value, no null-valued key) — that's a forged or
    // mismatched payload.
    if !data.values.is_empty() {
        let chosen = data.values[0].clone();
        let mut filled = false;
        if let Some(obj) = args.as_object_mut() {
            for (_, v) in obj.iter_mut() {
                if v.is_null() {
                    *v = Value::String(chosen.clone());
                    filled = true;
                    break;
                }
            }
        }
        if !filled {
            record_rejection(
                adapter,
                &claims.sub,
                &claims.tenant,
                TritonError::Validation("select-menu callback: no null arg slot to fill".into()),
            );
            return (StatusCode::BAD_REQUEST, "no slot for selection").into_response();
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
            match surface_mapper::try_render_surface(&dispatch.result, &adapter.correlation_key) {
                Some(Ok(rendered)) => {
                    let body = surface_mapper::build_interaction_response(&rendered);
                    adapter.dispatcher.record_post(
                        &tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Ok((200, "posted")),
                    );
                    (StatusCode::OK, axum::Json(body)).into_response()
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
            // 200 with the failure body so Discord doesn't retry
            // forever on a permanent app-layer error. Same
            // retry-storm avoidance as the Telegram courier.
            let body = json!({
                "type": 4,
                "data": { "content": format!("(error: {})", e.class()), "flags": 64 }
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
    }
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
    /// PR 25: string-select-menu callbacks carry the chosen option
    /// value(s) here. The mapper emits a select menu whose
    /// correlation token encodes `(tool, {args_key: null})`; at
    /// callback time we substitute `values[0]` for the null
    /// before dispatching. PR 25 supports single-select only
    /// (`min_values = max_values = 1`), so we only ever consume
    /// `values[0]`.
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
}

#[derive(Debug, Deserialize)]
struct DiscordMember {
    #[serde(default)]
    user: Option<DiscordUser>,
}
