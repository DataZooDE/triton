//! v0.2 secret resolution. Adapters declare credentials as
//! [`SecretField`] values: an inline literal (dev only), an
//! `env://<VARNAME>` reference (resolved from the process environment —
//! the production-safe shape on a Vault-less substrate), or a
//! `vault://<path>#<field>` reference. The resolver materialises any of
//! these into a `String` at boot.
//!
//! `env://` refs resolve regardless of which resolver is selected (no
//! backend needed), so they work on the substrate where secrets arrive
//! as container env (GCP Secret Manager → kamal `.kamal/secrets`).
//!
//! Two impls ship today:
//!
//! * [`LiteralResolver`] — refuses Vault refs. Selected when the
//!   substrate hasn't injected `TRITON_VAULT_URL` (+ an auth method).
//!   A manifest carrying Vault refs against this resolver fails boot,
//!   which is the FR-L-4 / M-SECRETS-1 contract.
//! * [`VaultKvResolver`] — calls Vault KV v2 over HTTP, presents a
//!   [`VaultToken`] in `X-Vault-Token`, decodes the `data.data.<field>`
//!   envelope. The token comes either from a static `TRITON_VAULT_TOKEN`
//!   or from Nomad workload identity (the binary logs in at
//!   `auth/<mount>/login`); see [`VaultToken`].
//!
//! The trait is `async` because Vault reads are HTTP; the dispatcher
//! never needs to call this at request time — secrets are resolved
//! once during boot and the resulting `String`s live in the adapter
//! struct.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use triton_manifest::SecretField;

mod vault_token;
pub use vault_token::{VaultAuthError, VaultToken};

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
            SecretField::Env { var } => resolve_env(var),
            SecretField::Vault { path, field } => Err(ResolveError::VaultNotConfigured {
                ref_string: format!("vault://{path}#{field}"),
            }),
        }
    }
}

/// Materialise an `env://<VARNAME>` ref from the process environment.
/// Backend-independent (no Vault, no network), so BOTH resolvers share
/// it — this is how the Vault-less substrate delivers secrets (GCP
/// Secret Manager → kamal `.kamal/secrets` → container env). A missing
/// or non-UTF-8 variable fails the boot closed (M-SECRETS-1).
fn resolve_env(var: &str) -> Result<String, ResolveError> {
    std::env::var(var).map_err(|_| ResolveError::EnvNotSet {
        var: var.to_string(),
    })
}

/// Resolves both literal and Vault refs. Vault refs are read via
/// `GET <base>/v1/<path>` with the configured `X-Vault-Token` and
/// the requested field plucked out of the KV v2 `data.data` map.
///
/// **KV v2 only.** Manifest refs must include the v2 `data/`
/// segment (e.g. `vault://kv/data/apps/.../telegram#secret`) — the
/// substrate Vault mount is KV v2 by convention. KV v1 mounts and
/// dynamic-secret engines (database/, transit/, etc.) aren't in
/// scope for v0.2; if they're ever needed, add a new resolver
/// variant rather than overloading this one.
pub struct VaultKvResolver {
    base: String,
    token: VaultToken,
    http: reqwest::Client,
}

impl VaultKvResolver {
    pub fn new(base_url: impl Into<String>, token: VaultToken) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                // Boot-time call; if Vault is dead, exit fast and
                // let Nomad reschedule rather than hang for minutes.
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    /// One KV v2 read. `Status { 401|403 }` is the retryable case
    /// (our Vault token may be revoked) — the caller invalidates and
    /// retries once.
    async fn fetch_once(&self, path: &str, field: &str) -> Result<String, ResolveError> {
        let url = format!("{}/v1/{}", self.base, path);
        let vault_token = self
            .token
            .get()
            .await
            .map_err(|e| ResolveError::Transport {
                url: url.clone(),
                detail: format!("vault auth: {e}"),
            })?;
        let resp = self
            .http
            .get(&url)
            .header("X-Vault-Token", &vault_token)
            .send()
            .await
            .map_err(|e| ResolveError::Transport {
                url: url.clone(),
                detail: e.to_string(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            return Err(ResolveError::Status {
                url: url.clone(),
                status: code,
                hint: vault_status_hint(code),
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
        let raw = inner.get(field).ok_or_else(|| ResolveError::MissingField {
            url: url.clone(),
            field: field.to_string(),
        })?;
        // Codex (PR 16 review) flagged: collapsing wrong-type
        // into MissingField hides the diagnosis. A KV v2
        // entry whose value is an object/array/null/number
        // is a manifest bug ("you stored a JSON object where
        // the resolver expects a string") and operators need
        // it labelled separately from "field not stored".
        let value = raw.as_str().ok_or_else(|| ResolveError::WrongType {
            url: url.clone(),
            field: field.to_string(),
            actual: json_type(raw),
        })?;
        Ok(value.to_string())
    }
}

#[async_trait]
impl SecretResolver for VaultKvResolver {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError> {
        match field {
            SecretField::Literal(s) => Ok(s.clone()),
            SecretField::Env { var } => resolve_env(var),
            SecretField::Vault { path, field } => match self.fetch_once(path, field).await {
                // Vault rejected our token — it may have been revoked
                // before its proactive refresh. Force a re-login and
                // retry once.
                Err(ResolveError::Status { status, .. }) if status == 401 || status == 403 => {
                    self.token.invalidate().await;
                    self.fetch_once(path, field).await
                }
                other => other,
            },
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
    #[error(
        "manifest declares `env://{var}` but the environment variable is unset or not UTF-8 — \
         the substrate must inject it (GCP Secret Manager → kamal `.kamal/secrets`)"
    )]
    EnvNotSet { var: String },
    #[error("vault transport error on {url}: {detail}")]
    Transport { url: String, detail: String },
    #[error("vault non-2xx on {url}: {status}{hint}")]
    Status {
        url: String,
        status: u16,
        /// Operator-facing diagnosis appended to the message; empty
        /// for statuses without a known common cause.
        hint: &'static str,
    },
    #[error("vault response decode failed on {url}: {detail}")]
    Decode { url: String, detail: String },
    #[error("vault response shape unexpected on {url}: {detail}")]
    Shape { url: String, detail: String },
    #[error("vault secret at {url} has no field `{field}`")]
    MissingField { url: String, field: String },
    #[error("vault secret at {url} field `{field}` is `{actual}`, expected string")]
    WrongType {
        url: String,
        field: String,
        actual: &'static str,
    },
}

/// Operator-facing hint appended to a Vault non-2xx error. Maps the
/// boot-time failures we actually hit to their likely cause — most
/// importantly the KV-path gotcha (`kv/data/...` API path in manifest
/// refs vs `kv/...` CLI path operators seed with) and the
/// "engine/policy not applied yet" case (a merged Terraform PR isn't
/// an applied one).
fn vault_status_hint(status: u16) -> &'static str {
    match status {
        404 => {
            " — no secret at this path. Common causes: the `kv/` engine isn't \
             enabled/applied on this Vault yet, the KV mount name differs, or the \
             path is wrong. Manifest refs use the API path \
             `vault://kv/data/<branch>#<field>`; operators seed with \
             `vault kv put kv/<branch> ...` (no `data/` segment)."
        }
        401 | 403 => {
            " — Vault rejected the token or denied the path. Check the token's \
             policy grants read on this branch and that the (workload-identity) \
             login succeeded."
        }
        _ => "",
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::{LiteralResolver, ResolveError, SecretResolver, vault_status_hint};
    use triton_manifest::SecretField;

    // A unique var name keeps this hermetic under the parallel test
    // runner (process-global env).
    const VAR: &str = "TRITON_SECRETS_TEST_ENV_REF_92F1";

    #[tokio::test]
    async fn literal_resolver_materialises_env_refs() {
        // env:// resolves with NO Vault configured — the substrate path.
        // SAFETY: single-threaded resolution of one uniquely-named var.
        unsafe {
            std::env::set_var(VAR, "the-secret-value");
        }
        let got = LiteralResolver
            .resolve(&SecretField::Env { var: VAR.into() })
            .await
            .expect("env ref resolves");
        assert_eq!(got, "the-secret-value");

        unsafe {
            std::env::remove_var(VAR);
        }
        let err = LiteralResolver
            .resolve(&SecretField::Env { var: VAR.into() })
            .await
            .expect_err("unset env var must fail closed");
        assert!(matches!(err, ResolveError::EnvNotSet { .. }), "{err:?}");
    }

    #[test]
    fn status_hint_diagnoses_common_boot_failures() {
        // 404: the engine/path gotcha gets the most actionable hint.
        let h404 = vault_status_hint(404);
        assert!(h404.contains("kv/") && h404.contains("kv/data/"));
        // 401/403: point at token/policy, not the path.
        assert!(vault_status_hint(403).contains("policy"));
        assert!(vault_status_hint(401).contains("policy"));
        // Anything else: no hint (don't guess).
        assert_eq!(vault_status_hint(500), "");
    }
}
