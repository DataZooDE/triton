//! v0.2 PR 34 — Signal adapter (via the signald daemon).
//!
//! Signal is the first chat-channel adapter whose I/O isn't an
//! HTTP webhook: signald listens on a Unix or TCP socket and
//! speaks line-delimited JSON. The adapter holds ONE persistent
//! connection (see [`signald_client`]) and reconnects with
//! exponential backoff on failure. There is no per-message
//! signature envelope — the trust boundary is the network path
//! (the daemon is tailnet-only). The manifest records this as
//! `signature: trusted_socket`; the adapter still resolves both
//! `signald_addr` and `account` at boot so a misconfigured deploy
//! fails closed (M-SECRETS-1).
//!
//! Wire shape (signald.org/articles/protocol):
//!
//! Subscribe (once, after connect):
//! ```json
//! { "type": "subscribe", "account": "+15551234567" }
//! ```
//! Receive event:
//! ```json
//! { "type": "IncomingMessage",
//!   "data": { "source": { "uuid": "...", "number": "..." },
//!             "data_message": { "body": "...", "timestamp": ... }}}
//! ```
//! Send:
//! ```json
//! { "type": "send", "username": "...",
//!   "recipientAddress": { "uuid": "...", "number": "..." },
//!   "messageBody": "..." }
//! ```
//! We don't wait for `send_results` — Telegram's PR 18 sendMessage
//! is similarly fire-and-forget; errors surface in audit at
//! `record_post` time.

pub mod signald_client;
pub mod surface_mapper;

pub use surface_mapper::{RenderedMessage, build_send_body};

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use triton_core::{Dispatcher, PostOutcome, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_secrets::{ResolveError, SecretResolver};

use signald_client::{SendError, Sender, SignaldAddr, SignaldEvent, spawn_connect_loop};

pub const PROTOCOL: &str = "messenger:signal";

/// Per-Signal-user claims resolved from the `sender_table`. Keyed by
/// the sender's UUID (the `source.uuid` field in the IncomingMessage
/// envelope). Phone numbers are deliberately not used as the key:
/// Signal explicitly designs the UUID as the durable identity, and
/// users rotate numbers.
#[derive(Debug, Clone, Deserialize)]
pub struct SenderClaims {
    pub sub: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub tenant: String,
}

/// Build artefacts the adapter holds. Constructed once at boot from
/// the manifest entry; immutable thereafter.
pub struct SignalAdapter {
    name: String,
    /// signald daemon address. Used to (re-)establish the persistent
    /// connection in the connect loop.
    signald_addr: SignaldAddr,
    /// The bot's Signal phone number (E.164 with leading `+`). Sent
    /// in the initial `subscribe` request and copied into every
    /// outbound `send.username`.
    account: String,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: signal`")]
    WrongKind,
    #[error("signal adapter limitation: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, ResolveError),
    #[error("identity.table failed to parse as sender JSON: {0}")]
    TableParse(String),
    #[error("inbound.signald_addr unparseable: {0}")]
    AddrParse(String),
}

impl SignalAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::Signal {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::TrustedSocket {
            return Err(BuildError::Unsupported(format!(
                "signal adapter requires `signature: trusted_socket`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "signal adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let addr_field = adapter
            .inbound
            .credentials
            .get("signald_addr")
            .ok_or(BuildError::MissingCredential("inbound.signald_addr"))?;
        let addr_str = resolver
            .resolve(addr_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.signald_addr", e))?;
        let signald_addr = SignaldAddr::parse(addr_str.trim()).map_err(BuildError::AddrParse)?;

        let account_field = adapter
            .inbound
            .credentials
            .get("account")
            .ok_or(BuildError::MissingCredential("inbound.account"))?;
        let account = resolver
            .resolve(account_field)
            .await
            .map_err(|e| BuildError::Resolve("inbound.account", e))?
            .trim()
            .to_string();
        if !account.starts_with('+') {
            return Err(BuildError::Unsupported(format!(
                "inbound.account must be E.164 with leading `+`; got `{account}`"
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

        // FR-L-6 / NFR-S-5: resolve correlation_key at boot even
        // though Signal has no native button primitive to feed it
        // — keeping the resolver call ensures a bad Vault ref fails
        // closed (mirrors Discord's outbound-token preflight).
        let _ = resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?;

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
            signald_addr,
            account,
            sender_table,
            dispatcher,
            rate_limit,
            per_tenant_limit,
        })
    }

    /// Spawn the signald connection task. The returned `JoinHandle`
    /// resolves once the connection loop has wound down after
    /// `shutdown` is cancelled (FR-L-2 graceful drain). The handle
    /// owns the inbound dispatch loop too — both tasks share the
    /// same `Sender` for outbound writes.
    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (events_tx, mut events_rx) = mpsc::channel::<SignaldEvent>(256);
            let (sender, conn_join) =
                spawn_connect_loop(self.signald_addr.clone(), events_tx, shutdown.clone());

            // Inbound dispatch loop. Consumes events from signald and
            // routes IncomingMessage envelopes through the dispatcher.
            // Connected events trigger a (re-)subscribe so the
            // adapter resumes message delivery across reconnects.
            while let Some(event) = tokio::select! {
                ev = events_rx.recv() => ev,
                _ = shutdown.cancelled() => None,
            } {
                match event {
                    SignaldEvent::Connected => {
                        let body = json!({
                            "type": "subscribe",
                            "account": self.account,
                        });
                        match sender.send(&body).await {
                            Ok(()) => {
                                tracing::info!(
                                    adapter = %self.name,
                                    "signald: subscribed",
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    adapter = %self.name,
                                    "signald: subscribe failed: {e}",
                                );
                            }
                        }
                    }
                    SignaldEvent::Line(value) => {
                        if let Err(e) = self.handle_event(&value, &sender).await {
                            tracing::warn!(
                                adapter = %self.name,
                                "signald: handle_event failed: {e}",
                            );
                        }
                    }
                }
            }

            // Wait for the connection task to wind down its socket
            // shutdown before this task itself exits — keeps the
            // drain sequence ordered (FR-L-2).
            let _ = conn_join.await;
        })
    }

    /// Route one parsed signald event. Non-IncomingMessage events
    /// are silently ignored (typing indicators, receipts, version
    /// banners, send_results — none of which carry user-driven
    /// content into the dispatcher). Returns `Err` only when the
    /// adapter itself had nothing to send (e.g. an envelope shape
    /// surprise); in normal flow, rejection paths still return
    /// `Ok(())` because the audit emitter handled the outcome.
    async fn handle_event(&self, event: &Value, sender: &Sender) -> Result<(), String> {
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if event_type != "IncomingMessage" {
            // Quietly drop. The protocol stream carries a bunch of
            // non-message events we don't want to log per-line.
            return Ok(());
        }
        let data = event.get("data").unwrap_or(&Value::Null);
        let body = data
            .get("data_message")
            .and_then(|m| m.get("body"))
            .and_then(|b| b.as_str())
            .unwrap_or("");
        if body.is_empty() {
            // Receipts, typing notifications, sync messages — all
            // arrive as IncomingMessage but with no body. Skip
            // silently; do NOT audit (no decision was made).
            return Ok(());
        }

        // PR 24-shaped first-tier rate limit. Same placement as the
        // HTTP-webhook adapters: consumed BEFORE sender lookup so a
        // noisy sender can't waste cycles on table probes.
        if let Err(retry_after) = self.rate_limit.try_take() {
            // No HTTP path to return — silent drop on the wire,
            // audit visibility through `record_rejection`.
            record_rejection(
                self,
                "-",
                "-",
                TritonError::RateLimited(format!(
                    "signal adapter `{}` rate limit hit; retry in {:.2}s",
                    self.name, retry_after
                )),
            );
            return Ok(());
        }

        let source_uuid = data
            .get("source")
            .and_then(|s| s.get("uuid"))
            .and_then(|u| u.as_str())
            .unwrap_or("");
        let source_number = data
            .get("source")
            .and_then(|s| s.get("number"))
            .and_then(|n| n.as_str());
        if source_uuid.is_empty() {
            // No UUID on the source — Signal envelopes always carry
            // one; absence is a malformed event we can't act on.
            // Audit as validation; this surfaces a protocol skew.
            record_rejection(
                self,
                "-",
                "-",
                TritonError::Validation(
                    "IncomingMessage without source.uuid (envelope shape skew)".into(),
                ),
            );
            return Ok(());
        }

        let Some(claims) = self.sender_table.get(source_uuid).cloned() else {
            // Unknown sender → silent drop on the wire (no HTTP
            // response to return), but auditable as `phase: rejected
            // / result: error:auth`. Sender UUIDs are logged at info
            // (low-PII — they're random opaque ids); the body itself
            // never leaves trace level (privacy).
            tracing::info!(
                adapter = %self.name,
                sender_uuid = %source_uuid,
                body_len = body.len(),
                "signal: unknown sender; dropping",
            );
            record_rejection(
                self,
                "-",
                "-",
                TritonError::Auth(format!("unknown sender uuid {source_uuid}")),
            );
            return Ok(());
        };

        // Per-tenant fair-share (PR 28 NFR-P-3 second tier). Bucket
        // key is the verified tenant — never the platform UUID.
        if let Err(retry_after) = self.per_tenant_limit.try_take(&claims.tenant) {
            record_rejection(
                self,
                &claims.sub,
                &claims.tenant,
                TritonError::RateLimited(format!(
                    "tenant `{}` rate limit hit on adapter `{}`; retry in {:.2}s",
                    claims.tenant, self.name, retry_after
                )),
            );
            return Ok(());
        }

        let principal = Principal {
            sub: claims.sub.clone(),
            scopes: claims.scopes.clone(),
            tenant: claims.tenant.clone(),
            raw_token: String::new(),
            trace_id: uuid::Uuid::new_v4().to_string(),
        };
        let (tool_name, args) = route_command(body);
        let principal_for_post = principal.clone();
        tracing::info!(
            adapter = %self.name,
            sender_uuid = %source_uuid,
            tool = tool_name,
            body_len = body.len(),
            "signal: dispatching command",
        );
        // Bodies only at trace per task constraint.
        tracing::trace!(adapter = %self.name, body = %body);

        let result = self
            .dispatcher
            .invoke(tool_name, args, principal, PROTOCOL)
            .await;
        match result {
            Ok(dispatch) => match render_dispatch_result(&dispatch.result) {
                Ok(rendered) => {
                    if rendered.deferred_buttons > 0 {
                        tracing::warn!(
                            adapter = %self.name,
                            tool = tool_name,
                            deferred_buttons = rendered.deferred_buttons,
                            "signal: button components deferred (no button primitive on signald)",
                        );
                    }
                    if rendered.deferred_selections > 0 {
                        tracing::warn!(
                            adapter = %self.name,
                            tool = tool_name,
                            deferred_selections = rendered.deferred_selections,
                            "signal: Selection components deferred",
                        );
                    }
                    if rendered.deferred_forms > 0 {
                        tracing::warn!(
                            adapter = %self.name,
                            tool = tool_name,
                            deferred_forms = rendered.deferred_forms,
                            "signal: Form components deferred",
                        );
                    }
                    if rendered.deferred_dashboards > 0 {
                        tracing::warn!(
                            adapter = %self.name,
                            tool = tool_name,
                            deferred_dashboards = rendered.deferred_dashboards,
                            "signal: Dashboard components deferred (rasteriser not yet wired)",
                        );
                    }
                    if rendered.truncated {
                        tracing::warn!(
                            adapter = %self.name,
                            tool = tool_name,
                            cap_bytes = surface_mapper::SIGNAL_TEXT_MAX_BYTES,
                            "signal: rendered text exceeded cap; truncated",
                        );
                    }
                    self.post_back(
                        sender,
                        &principal_for_post,
                        tool_name,
                        source_uuid,
                        source_number,
                        rendered,
                    )
                    .await;
                }
                Err(surface_mapper::RenderError::EmptyAfterRender) => {
                    tracing::warn!(
                        adapter = %self.name,
                        tool = tool_name,
                        "signal: empty surface; skipping post-back",
                    );
                    let provider =
                        TritonError::Provider("signal surface mapper: empty surface".into());
                    self.dispatcher.record_post(
                        tool_name,
                        PROTOCOL,
                        &principal_for_post,
                        0,
                        Err((&provider, 0, PostOutcome::Dropped, None)),
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    adapter = %self.name,
                    error = %e,
                    class = %e.class(),
                    "signal: tool dispatch failed",
                );
                // No outbound send (we don't ship error text back
                // to the user on Signal — surface mapper has
                // nothing to render). Dispatcher already audited
                // the failure.
            }
        }
        Ok(())
    }

    async fn post_back(
        &self,
        sender: &Sender,
        principal: &Principal,
        tool_name: &str,
        recipient_uuid: &str,
        recipient_number: Option<&str>,
        rendered: RenderedMessage,
    ) {
        let body = build_send_body(&self.account, recipient_uuid, recipient_number, &rendered);
        let start = std::time::Instant::now();
        let outcome = sender.send(&body).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        match outcome {
            Ok(()) => {
                self.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    // signald is fire-and-forget; we don't get a
                    // status code from the protocol. Use 200/posted
                    // to mirror the other adapters' "happy path"
                    // shape.
                    Ok((200, PostOutcome::Posted, None)),
                );
            }
            Err(e) => {
                let (label, http_status) = match &e {
                    SendError::Disconnected => (PostOutcome::Retry, 0u16),
                    SendError::Io(_) => (PostOutcome::Retry, 0u16),
                    SendError::Encode(_) => (PostOutcome::Dropped, 0u16),
                };
                tracing::warn!(
                    adapter = %self.name,
                    tool = tool_name,
                    "signal: courier failed: {e}",
                );
                let provider = TritonError::Provider(format!("signal courier: {e}"));
                self.dispatcher.record_post(
                    tool_name,
                    PROTOCOL,
                    principal,
                    latency_ms,
                    Err((&provider, http_status, label, None)),
                );
            }
        }
    }
}

/// Same shape as Telegram's `route_command`: a leading `/` is the
/// command marker; the first whitespace-separated token names the
/// tool; the rest is the argument shape (`subject` for `narrate`,
/// `message` for `echo`). Unknown commands fall through to `echo`
/// so the user sees their raw text echoed back, which makes the
/// "command not recognised" path observable rather than silent.
fn route_command(text: &str) -> (&'static str, Value) {
    if let Some(rest) = text.strip_prefix('/') {
        let (tool, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        match tool {
            "narrate" => return ("narrate", json!({ "subject": subject })),
            "echo" => return ("echo", json!({ "message": subject })),
            _ => {}
        }
    }
    ("echo", json!({ "message": text }))
}

fn render_dispatch_result(result: &Value) -> Result<RenderedMessage, surface_mapper::RenderError> {
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

fn record_rejection(adapter: &SignalAdapter, sub: &str, tenant: &str, e: TritonError) {
    adapter.dispatcher.record_rejection(
        &adapter.name,
        PROTOCOL,
        sub,
        tenant,
        &uuid::Uuid::new_v4().to_string(),
        &e,
    );
}
