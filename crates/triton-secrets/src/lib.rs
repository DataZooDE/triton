//! v0.2 secret resolution. Adapters declare credentials as
//! [`SecretField`] values: either an inline literal (dev only) or a
//! `vault://<path>#<field>` reference. The resolver materialises
//! either shape into a `String` at boot.
//!
//! Two impls ship today:
//!
//! * [`LiteralResolver`] — refuses Vault refs. Selected when the
//!   substrate hasn't injected `TRITON_VAULT_URL` / `_TOKEN`. A
//!   manifest carrying Vault refs against this resolver fails boot,
//!   which is the FR-L-4 / M-SECRETS-1 contract.
//! * [`VaultKvResolver`] — calls Vault KV v2 over HTTP, presents the
//!   stored Triton vault token in `X-Vault-Token`, decodes the
//!   `data.data.<field>` envelope.
//!
//! The trait is `async` because Vault reads are HTTP; the dispatcher
//! never needs to call this at request time — secrets are resolved
//! once during boot and the resulting `String`s live in the adapter
//! struct.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use triton_manifest::SecretField;

/// Resolve a [`SecretField`] (literal or Vault ref) into the raw
/// secret string the adapter will use.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError>;
}

/// Refuses Vault refs. Used when no Vault is configured — the
/// substrate has guaranteed the manifest stays literal-only.
pub struct LiteralResolver;

#[async_trait]
impl SecretResolver for LiteralResolver {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError> {
        match field {
            SecretField::Literal(s) => Ok(s.clone()),
            SecretField::Vault { path, field } => Err(ResolveError::VaultNotConfigured {
                ref_string: format!("vault://{path}#{field}"),
            }),
        }
    }
}

/// Resolves both literal and Vault refs. Vault refs are read via
/// `GET <base>/v1/<path>` with the configured `X-Vault-Token` and
/// the requested field plucked out of the KV v2 `data.data` map.
pub struct VaultKvResolver {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl VaultKvResolver {
    pub fn new(base_url: impl Into<String>, vault_token: impl Into<String>) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            token: vault_token.into(),
            http: reqwest::Client::builder()
                // Boot-time call; if Vault is dead, exit fast and
                // let Nomad reschedule rather than hang for minutes.
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[async_trait]
impl SecretResolver for VaultKvResolver {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError> {
        match field {
            SecretField::Literal(s) => Ok(s.clone()),
            SecretField::Vault { path, field } => {
                let url = format!("{}/v1/{}", self.base, path);
                let resp = self
                    .http
                    .get(&url)
                    .header("X-Vault-Token", &self.token)
                    .send()
                    .await
                    .map_err(|e| ResolveError::Transport {
                        url: url.clone(),
                        detail: e.to_string(),
                    })?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(ResolveError::Status {
                        url: url.clone(),
                        status: status.as_u16(),
                    });
                }
                let body: Value = resp.json().await.map_err(|e| ResolveError::Decode {
                    url: url.clone(),
                    detail: e.to_string(),
                })?;
                let inner = body
                    .get("data")
                    .and_then(|d| d.get("data"))
                    .ok_or_else(|| ResolveError::Shape {
                        url: url.clone(),
                        detail: "missing `data.data` envelope (KV v1 mount?)".into(),
                    })?;
                let value = inner
                    .get(field.as_str())
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ResolveError::MissingField {
                        url: url.clone(),
                        field: field.clone(),
                    })?;
                Ok(value.to_string())
            }
        }
    }
}

/// Errors the resolver can surface at boot. Every variant exits
/// the binary non-zero (M-SECRETS-1 / FR-L-4): the substrate must
/// see a misconfigured deploy fail closed.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("manifest declares Vault ref `{ref_string}` but no resolver is configured")]
    VaultNotConfigured { ref_string: String },
    #[error("vault transport error on {url}: {detail}")]
    Transport { url: String, detail: String },
    #[error("vault non-2xx on {url}: {status}")]
    Status { url: String, status: u16 },
    #[error("vault response decode failed on {url}: {detail}")]
    Decode { url: String, detail: String },
    #[error("vault response shape unexpected on {url}: {detail}")]
    Shape { url: String, detail: String },
    #[error("vault secret at {url} has no field `{field}`")]
    MissingField { url: String, field: String },
}
