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

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use triton_core::{Dispatcher, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SecretField, SignatureScheme};

const PROTOCOL: &str = "messenger:telegram";
const HEADER_SECRET: &str = "X-Telegram-Bot-Api-Secret-Token";

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

/// Build artefacts the adapter holds. Constructed once at boot
/// from the manifest entry; immutable after that.
pub struct TelegramAdapter {
    name: String,
    secret_token: String,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
}

impl TelegramAdapter {
    /// Read a manifest [`Adapter`] of `kind: telegram`, pull every
    /// secret it needs out of the manifest's literal/Vault refs,
    /// and produce a runnable adapter. PR 13 only handles literal
    /// credentials; PR 14 wires a Vault resolver.
    pub fn from_manifest(
        name: &str,
        adapter: &Adapter,
        dispatcher: Arc<Dispatcher>,
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

        let secret_token =
            literal(adapter.inbound.credentials.get("secret"), "inbound.secret")?.to_string();

        let table_json = literal(adapter.identity.credentials.get("table"), "identity.table")?;
        let sender_table: HashMap<String, SenderClaims> =
            serde_json::from_str(table_json).map_err(|e| BuildError::TableParse(e.to_string()))?;

        Ok(Self {
            name: name.to_string(),
            secret_token,
            sender_table,
            dispatcher,
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

fn literal<'a>(field: Option<&'a SecretField>, label: &'static str) -> Result<&'a str, BuildError> {
    match field {
        Some(SecretField::Literal(s)) => Ok(s.as_str()),
        Some(SecretField::Vault { .. }) => Err(BuildError::VaultUnsupported(label)),
        None => Err(BuildError::MissingCredential(label)),
    }
}

/// Errors building the adapter from a manifest entry. Surfaced to
/// the operator at boot. `VaultUnsupported` is recoverable — the
/// caller should warn-and-skip; PR 14 wires the Vault resolver. All
/// other variants exit non-zero so misconfiguration cannot serve
/// traffic with degraded security.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: telegram`")]
    WrongKind,
    #[error("PR 13 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("credential field `{0}` is a Vault reference; PR 14 will resolve these")]
    VaultUnsupported(&'static str),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    #[serde(default)]
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    from: TelegramUser,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: u64,
}

async fn handle_webhook(
    State(adapter): State<Arc<TelegramAdapter>>,
    headers: HeaderMap,
    Json(update): Json<TelegramUpdate>,
) -> Response {
    // Signature check first (FR-I-8 / M-SIG-1). Constant-time
    // equality so an attacker can't timing-leak the secret.
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

    // PR 13 hardcoded routing: every text message goes to `echo`
    // with the body as the `message` arg. The real Telegram
    // command parser (e.g. `/echo`, `/stats`) lands in a later PR
    // with the surface mapper, where it gets shared with Discord.
    let args = json!({ "message": text });
    match adapter
        .dispatcher
        .invoke("echo", args, principal, PROTOCOL)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => {
            // Dispatcher already audited the failure. Echo the
            // status back to Telegram so we don't surface a stack
            // trace; Telegram will retry on 5xx.
            tracing::warn!(error = %e, "telegram tool dispatch failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
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
    if a.len() != b.len() {
        // Lengths differ — already a hard mismatch. Returning
        // early is fine: a length-mismatch oracle on a 32-byte
        // secret leaks at most 5 bits, which the operator can
        // close by enforcing a fixed-length secret_token.
        return false;
    }
    a.ct_eq(b).into()
}
