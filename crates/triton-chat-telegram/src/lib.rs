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

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
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

/// Build artefacts the adapter holds. Constructed once at boot
/// from the manifest entry; immutable after that.
pub struct TelegramAdapter {
    name: String,
    secret_token: String,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
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
        // boot or the binary refuses to start. `outbound.token` and
        // `correlation_key` aren't functionally consumed yet (PR 17
        // wires the courier; PR 18 wires HMAC correlation tokens),
        // but a misconfigured Vault ref in either field would
        // silently survive boot otherwise — and then surface as a
        // mid-traffic failure when the dependent PR ships.
        // Codex caught this gap in PR 16 review.
        if let Some(field) = adapter.outbound.credentials.get("token") {
            resolver
                .resolve(field)
                .await
                .map_err(|e| BuildError::Resolve("outbound.token", e))?;
        }
        resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?;

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
