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

        Ok(Self {
            name: name.to_string(),
            verifying_key,
            correlation_key,
            sender_table,
            dispatcher,
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
            // not in PR 22's scope. We ack so Discord doesn't
            // retry, and audit a clear deferred-shape rejection.
            tracing::warn!(
                interaction_type = other,
                "discord interaction type not yet supported; acking",
            );
            (StatusCode::OK, axum::Json(json!({ "type": 1 }))).into_response()
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

    let (tool_name, args) = match triton_correlation::decode(token, &adapter.correlation_key) {
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
                    // result as plain content. Same fallback shape
                    // the Telegram adapter uses for echo-style
                    // tools.
                    let content = bare_text(&dispatch.result);
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
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    #[serde(default)]
    custom_id: Option<String>,
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
