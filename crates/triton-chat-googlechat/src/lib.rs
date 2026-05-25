//! v0.2 PR 33 — Google Chat (formerly Hangouts Chat) adapter.
//!
//! Google Chat posts events to our webhook with a Google-signed JWT
//! in the `Authorization: Bearer …` header. Unlike Telegram (which
//! POSTs back via a courier) Google Chat reads the HTTP 200 body of
//! our webhook as the reply ("synchronous-response" pattern); for
//! PR 33 we use only this pattern so the adapter has no outbound
//! HTTP call.
//!
//! Scope (PR 33):
//!
//!   * Inbound auth: validate the JWT against Google's published
//!     certs (RS256, `iss == chat@system.gserviceaccount.com`,
//!     `aud == manifest.audience`). FR-I-8 constant-time semantics
//!     come from the cryptographic verifier itself, not byte
//!     comparisons we own — the only constant-time-relevant
//!     surface is "did the signature verify?" which jsonwebtoken
//!     answers via RSA verify.
//!   * Identity: `sender_table` lookup by the platform sender's
//!     `users/<id>` string. Same shape as Telegram + Discord.
//!   * Dispatch: text + narration only. Buttons / Selection /
//!     Form / Dashboard defer with counters (architecture.md §8.7
//!     — interactive Cards are a follow-up PR).
//!   * Audit pivot: `record_rejection` on refused inbounds,
//!     dispatcher invoke audits the `phase: dispatch`, and we call
//!     `record_post` with `latency_ms=0, http_status=200` for the
//!     inline response (the substrate has no per-post HTTP roundtrip
//!     to measure, but the audit shape must still carry one post
//!     line per delivered message).
//!
//! Layout mirrors Telegram (`lib.rs` + `surface_mapper.rs` +
//! `jwt_verifier.rs`). Adapter struct stays under the CLAUDE.md
//! §4 LOC budget; JWT details live in `jwt_verifier.rs`.

pub mod jwt_verifier;
pub mod surface_mapper;
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
use serde_json::Value;
use triton_core::{Dispatcher, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

use crate::jwt_verifier::{GoogleJwtVerifier, VerifierConfig};

pub const PROTOCOL: &str = "messenger:google_chat";

/// Per-Google-Chat-user claims resolved from the `sender_table`.
/// The table is keyed by the full `users/<id>` string Google sends
/// in `message.sender.name`.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

pub struct GoogleChatAdapter {
    name: String,
    verifier: Arc<GoogleJwtVerifier>,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
}

impl GoogleChatAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        jwks_uri: String,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::GoogleChat {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::GoogleOidcJwt {
            return Err(BuildError::Unsupported(format!(
                "google_chat adapter requires `signature: google_oidc_jwt`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "google_chat adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let aud_field = adapter
            .inbound
            .credentials
            .get("audience")
            .ok_or(BuildError::MissingCredential("inbound.audience"))?;
        let audience = resolver
            .resolve(aud_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.audience", e))?;
        if audience.is_empty() {
            return Err(BuildError::Unsupported(
                "inbound.audience MUST be non-empty".into(),
            ));
        }

        // FR-L-6 / NFR-S-5: every credential field MUST resolve at
        // boot. `outbound.token` is reserved for the future async
        // courier path (`https://chat.googleapis.com/...`); we still
        // resolve it now so a misconfigured Vault ref fails closed.
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

        // `correlation_key` isn't functionally consumed in PR 33
        // (no Buttons mean no HMAC correlation tokens) but we still
        // resolve it so a bad Vault ref fails closed at boot.
        resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?;

        // PR 28 headroom rationale: see triton-chat-telegram.
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
        let verifier = Arc::new(GoogleJwtVerifier::new(VerifierConfig::new(
            jwks_uri,
            audience.clone(),
        )));
        Ok(Self {
            name: name.to_string(),
            verifier,
            sender_table,
            dispatcher,
            rate_limit,
            per_tenant_limit,
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

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: google_chat`")]
    WrongKind,
    #[error("PR 33 limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
}

#[derive(Debug, Deserialize)]
struct GoogleChatEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: Option<GoogleChatMessage>,
}

#[derive(Debug, Deserialize)]
struct GoogleChatMessage {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    sender: Option<GoogleChatSender>,
}

#[derive(Debug, Deserialize)]
struct GoogleChatSender {
    /// Full resource name `users/<id>`. Adapter uses this verbatim
    /// as the sender_table key — exposing the leading `users/` in
    /// the key lets operators tell at a glance which entries are
    /// human senders (other Google Chat actors would use
    /// `bots/<id>`).
    #[serde(default)]
    name: Option<String>,
}

async fn handle_webhook(
    State(adapter): State<Arc<GoogleChatAdapter>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // FR-I-8 — verify the JWT FIRST, before parsing the body. The
    // JWT is in the header; we never read the body for an
    // unauthenticated request. Constant-time signature verification
    // is the cryptographic property of RSA-verify, not a manual
    // byte comparison.
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let raw = match auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    {
        Some(t) if !t.is_empty() => t.trim(),
        _ => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Auth("missing or malformed Authorization header".into()),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    let _claims = match adapter.verifier.verify(raw).await {
        Ok(c) => c,
        Err(e) => {
            // Never log the JWT body. Map the typed error to a
            // short reason so the audit pivot sees `error:auth`
            // without the token contents.
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Auth(format!("google jwt verify: {e}")),
            );
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };

    // NFR-P-3 first tier (adapter-wide rate limit). Consumed AFTER
    // the JWT check (so attackers can't waste tokens with bogus
    // bearers) but BEFORE body parse or sender resolution.
    if let Err(retry_after) = adapter.rate_limit.try_take() {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::RateLimited(format!(
                "google_chat adapter `{}` rate limit hit; retry in {:.2}s",
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

    let event: GoogleChatEvent = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            record_rejection(
                &adapter,
                "-",
                "-",
                TritonError::Validation(format!("malformed event body: {e}")),
            );
            return (StatusCode::BAD_REQUEST, "malformed").into_response();
        }
    };

    // Non-MESSAGE events (ADDED_TO_SPACE, REMOVED_FROM_SPACE,
    // CARD_CLICKED) are out of scope for PR 33. We ack 200 with an
    // empty body so Google doesn't retry; no dispatch, no rejection
    // audit (it's a normal opt-out, not an error).
    if event.kind != "MESSAGE" {
        return (
            StatusCode::OK,
            axum::Json(Value::Object(Default::default())),
        )
            .into_response();
    }

    let Some(message) = event.message else {
        // Type=MESSAGE without a message body is malformed; refuse.
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Validation("MESSAGE event missing message body".into()),
        );
        return (StatusCode::BAD_REQUEST, "missing message").into_response();
    };
    let Some(text) = message.text.as_deref().filter(|s| !s.is_empty()) else {
        // Empty text — Google sometimes sends these for image/attach
        // ments. We don't handle those in PR 33; ack and ignore.
        return (
            StatusCode::OK,
            axum::Json(Value::Object(Default::default())),
        )
            .into_response();
    };
    let sender_name = message
        .sender
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .unwrap_or("");
    let Some(claims) = adapter.sender_table.get(sender_name) else {
        record_rejection(
            &adapter,
            "-",
            "-",
            TritonError::Auth(format!("unknown sender `{sender_name}`")),
        );
        return (StatusCode::UNAUTHORIZED, "unknown sender").into_response();
    };

    // NFR-P-3 second tier: per-tenant fair-share, keyed by the
    // verified tenant id (never the platform sender name).
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

    let principal = Principal {
        sub: claims.sub.clone(),
        scopes: claims.scopes.clone(),
        tenant: claims.tenant.clone(),
        raw_token: String::new(),
        trace_id: uuid::Uuid::new_v4().to_string(),
    };
    let principal_for_post = principal.clone();
    let (tool_name, args) = route_command(text);

    let result = adapter
        .dispatcher
        .invoke(tool_name, args, principal, PROTOCOL)
        .await;
    match result {
        Ok(dispatch) => match render_dispatch_result(&dispatch.result) {
            Ok(rendered) => {
                if rendered.deferred_buttons
                    + rendered.deferred_selections
                    + rendered.deferred_forms
                    + rendered.deferred_dashboards
                    > 0
                {
                    tracing::warn!(
                        tool = tool_name,
                        deferred_buttons = rendered.deferred_buttons,
                        deferred_selections = rendered.deferred_selections,
                        deferred_forms = rendered.deferred_forms,
                        deferred_dashboards = rendered.deferred_dashboards,
                        "google_chat surface mapper: interactive components deferred (Cards not yet wired)",
                    );
                }
                if rendered.truncated {
                    tracing::warn!(
                        tool = tool_name,
                        cap_bytes = surface_mapper::GOOGLE_CHAT_TEXT_MAX_BYTES,
                        "google_chat surface mapper: rendered text exceeded cap; truncated",
                    );
                }
                let body = surface_mapper::build_inline_response(&rendered);
                adapter.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Ok((200, "posted")),
                );
                (StatusCode::OK, axum::Json(body)).into_response()
            }
            Err(surface_mapper::RenderError::EmptyAfterRender) => {
                tracing::warn!(
                    tool = tool_name,
                    "google_chat surface mapper: empty surface; skipping inline response",
                );
                let provider =
                    TritonError::Provider("google_chat surface mapper: empty surface".into());
                adapter.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    &principal_for_post,
                    0,
                    Err((&provider, 0, "dropped")),
                );
                // Still 200 so Google doesn't retry; empty body.
                (
                    StatusCode::OK,
                    axum::Json(Value::Object(Default::default())),
                )
                    .into_response()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, class = %e.class(), "google_chat dispatch failed");
            // Permanent app-layer failures ack 200 to avoid retry
            // storms — Google Chat keeps trying non-2xx for a long
            // while. Transient infra faults earn a 502 so the
            // platform can re-deliver.
            let status = match &e {
                TritonError::Provider(_) => StatusCode::BAD_GATEWAY,
                _ => StatusCode::OK,
            };
            // Audit the failure path as `phase: post` with the
            // observed class so operators see one consistent shape
            // across success + failure.
            adapter.dispatcher.record_post(
                tool_name,
                PROTOCOL,
                &principal_for_post,
                0,
                Err((&e, 0, "error_response")),
            );
            (
                status,
                axum::Json(serde_json::json!({ "text": format!("(error: {})", e.class()) })),
            )
                .into_response()
        }
    }
}

/// Strip a leading `@bot ` mention if present, then route by the
/// first word. Mirrors Telegram's `route_command`: `/<tool> <rest>`
/// goes to `tool` with `{ subject: rest }` for narrate, otherwise
/// falls through to `echo`.
fn route_command(text: &str) -> (&'static str, Value) {
    let trimmed = text
        .trim_start_matches("@bot ")
        .trim_start_matches("@bot")
        .trim_start();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        if tool == "narrate" {
            return ("narrate", serde_json::json!({ "subject": subject }));
        }
    }
    ("echo", serde_json::json!({ "message": trimmed }))
}

fn render_dispatch_result(
    result: &serde_json::Value,
) -> Result<RenderedMessage, surface_mapper::RenderError> {
    if let Some(r) = surface_mapper::try_render_surface(result) {
        return r;
    }
    // Fall back to the bare-text path so echo-shaped results
    // (`{"echo": "..."}`) still render without an A2UI envelope.
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

fn record_rejection(adapter: &GoogleChatAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}
