//! The outbound half: an [`OutboundCourier`] that delivers an
//! agent-initiated email over a transactional-email HTTP API.
//!
//! It is deliberately much simpler than the chat couriers: email has no
//! inbound webhook, no service window, no message templates, and no
//! interactive-callback signing (its buttons are plain links). `deliver`
//! renders the surface through the SAME [`crate::surface_mapper`] the preview
//! uses, then POSTs `{from, to, subject, html, text}` to the provider. Audit
//! stays in the dispatcher via [`Dispatcher::record_post`] (ADR-6).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use triton_core::audit::PostOutcome;
use triton_core::{Dispatcher, OutboundCourier, OutboundRequest, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, OutboundKind};
use triton_secrets::{ResolveError, SecretResolver};

use crate::surface_mapper;

/// Audit protocol label for email sends.
pub const PROTOCOL: &str = "email";
/// The synthetic tool name a proactive send audits under (mirrors the chat
/// couriers' `outbound`).
const OUTBOUND_TOOL: &str = "outbound";

/// Configuration for the outbound courier. `api_base` is the transactional
/// email provider's endpoint (env `TRITON_EMAIL_API_BASE`, pointed at the
/// in-repo fake in tests); the courier POSTs to `{api_base}/send`.
#[derive(Debug, Clone)]
pub struct CourierConfig {
    pub api_base: String,
    pub timeout: Duration,
}

impl Default for CourierConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.email.example".to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Per-recipient claims resolved from the `sender_table` (recipient email →
/// tenant binding), mirroring the chat adapters' sender table.
#[derive(Debug, Clone, Deserialize)]
struct RecipientClaims {
    #[serde(default)]
    #[allow(dead_code)]
    sub: String,
    #[serde(default)]
    #[allow(dead_code)]
    scopes: Vec<String>,
    tenant: String,
}

/// Built email adapter; immutable after boot.
pub struct EmailAdapter {
    #[allow(dead_code)]
    name: String,
    /// The `From:` address stamped on every send (manifest `outbound.from`).
    from: String,
    /// The transactional-email API key (manifest `outbound.token`).
    api_key: String,
    /// recipient-email → tenant binding (manifest `identity.table`).
    sender_table: HashMap<String, RecipientClaims>,
    dispatcher: Arc<Dispatcher>,
    http: reqwest::Client,
    api_base: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("adapter is not declared `kind: email`")]
    WrongKind,
    #[error("unsupported email adapter config: {0}")]
    Unsupported(String),
    #[error("missing credential field `{0}`")]
    MissingCredential(&'static str),
    #[error("could not resolve credential field `{0}`: {1}")]
    Resolve(&'static str, #[source] ResolveError),
    #[error("identity.table failed to parse as recipient JSON: {0}")]
    TableParse(String),
    #[error("could not build the email HTTP client: {0}")]
    Http(String),
}

impl EmailAdapter {
    /// Build from a manifest `kind: email` adapter. Reads `outbound.token`
    /// (API key) + `outbound.from` (sender address) + `identity.table`
    /// (recipient→tenant JSON), all through the secret resolver so a bad
    /// reference fails closed at boot.
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        config: CourierConfig,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::Email {
            return Err(BuildError::WrongKind);
        }
        if adapter.outbound.kind != OutboundKind::RestApi {
            return Err(BuildError::Unsupported(format!(
                "email adapter requires `outbound.kind: rest_api`; got {:?}",
                adapter.outbound.kind
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "email adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let token_field = adapter
            .outbound
            .credentials
            .get("token")
            .ok_or(BuildError::MissingCredential("outbound.token"))?;
        let api_key = resolver
            .resolve(token_field)
            .await
            .map_err(|e| BuildError::Resolve("outbound.token", e))?;

        let from_field = adapter
            .outbound
            .credentials
            .get("from")
            .ok_or(BuildError::MissingCredential("outbound.from"))?;
        let from = resolver
            .resolve(from_field)
            .await
            .map_err(|e| BuildError::Resolve("outbound.from", e))?;

        let table_field = adapter
            .identity
            .credentials
            .get("table")
            .ok_or(BuildError::MissingCredential("identity.table"))?;
        let table_json = resolver
            .resolve(table_field)
            .await
            .map_err(|e| BuildError::Resolve("identity.table", e))?;
        // Email addresses are matched case-insensitively (the domain always is,
        // and practically the local part too), so normalise the table keys to
        // lowercase at build; `authorize` normalises the recipient the same way.
        let sender_table: HashMap<String, RecipientClaims> =
            serde_json::from_str::<HashMap<String, RecipientClaims>>(&table_json)
                .map_err(|e| BuildError::TableParse(e.to_string()))?
                .into_iter()
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v))
                .collect();

        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| BuildError::Http(e.to_string()))?;

        Ok(Self {
            name: name.to_string(),
            from,
            api_key,
            sender_table,
            dispatcher,
            http,
            api_base: config.api_base,
        })
    }
}

/// Strip the API key out of an error string so a transport error can never
/// leak the bearer into a log line.
fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        return s.to_string();
    }
    s.replace(secret, "***")
}

#[async_trait]
impl OutboundCourier for EmailAdapter {
    fn protocol(&self) -> &'static str {
        PROTOCOL
    }

    /// #113 recipient/tenant binding: `to` MUST be a known recipient whose
    /// tenant matches the caller's — an agent can only email its own tenant's
    /// users.
    async fn authorize(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        match self.sender_table.get(&req.to.trim().to_ascii_lowercase()) {
            Some(claims) if claims.tenant == principal.tenant => Ok(()),
            Some(_) => Err(TritonError::Forbidden(format!(
                "recipient {} is not in tenant `{}`",
                req.to, principal.tenant
            ))),
            None => Err(TritonError::Forbidden(format!(
                "recipient {} is not a known recipient for this adapter",
                req.to
            ))),
        }
    }

    /// Render the surface to an email and POST it to the provider. Email has
    /// no service window, so there's no template path — a rendered surface
    /// ships directly.
    async fn deliver(
        &self,
        req: &OutboundRequest,
        principal: &Principal,
    ) -> Result<(), TritonError> {
        let rendered = match surface_mapper::try_render_surface(&req.result) {
            Some(Ok(r)) => r,
            Some(Err(_)) => {
                return Err(TritonError::Validation(
                    "outbound surface rendered to nothing".into(),
                ));
            }
            None => {
                return Err(TritonError::Validation(
                    "outbound result is not an A2UI surface".into(),
                ));
            }
        };
        let body = json!({
            "from": self.from,
            "to": req.to,
            "subject": rendered.subject,
            "html": rendered.html,
            "text": rendered.text,
        });
        let url = format!("{}/send", self.api_base);
        let start = Instant::now();
        let outcome = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await;
        let latency = start.elapsed().as_millis() as u64;
        match outcome {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    self.dispatcher.record_post(
                        OUTBOUND_TOOL,
                        PROTOCOL,
                        principal,
                        latency,
                        Ok((status, PostOutcome::Posted, None)),
                    );
                    Ok(())
                } else {
                    let label = if status == 429 || status >= 500 {
                        PostOutcome::Retry
                    } else {
                        PostOutcome::Dropped
                    };
                    let err = TritonError::Provider(format!("email API returned {status}"));
                    self.dispatcher.record_post(
                        OUTBOUND_TOOL,
                        PROTOCOL,
                        principal,
                        latency,
                        Err((&err, status, label, None)),
                    );
                    Err(err)
                }
            }
            Err(e) => {
                let err = TritonError::Provider(format!(
                    "email transport error: {}",
                    redact(&e.to_string(), &self.api_key)
                ));
                self.dispatcher.record_post(
                    OUTBOUND_TOOL,
                    PROTOCOL,
                    principal,
                    latency,
                    Err((&err, 0, PostOutcome::Retry, None)),
                );
                Err(err)
            }
        }
    }
}
