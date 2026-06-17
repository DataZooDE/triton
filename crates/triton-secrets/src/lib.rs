//! v0.2 secret resolution. Adapters declare credentials as
//! [`SecretField`] values: an inline literal (dev only) or an
//! `env://<VARNAME>` reference resolved from the process environment —
//! the production-safe shape on the Vault-less Kamal substrate, where
//! secrets arrive as container env (GCP Secret Manager → kamal
//! `.kamal/secrets`).
//!
//! Vault was decommissioned with the move off the HashiCorp stack, so
//! `vault://` refs no longer resolve: a manifest carrying one fails
//! boot closed (M-SECRETS-1 / FR-L-4), pointing the operator at the
//! `env://` replacement.
//!
//! The trait is `async` to keep the seam stable, even though the
//! surviving resolver needs no I/O; the dispatcher never calls this at
//! request time — secrets are resolved once during boot and the
//! resulting `String`s live in the adapter struct.

use async_trait::async_trait;
use triton_manifest::SecretField;

/// Resolve a [`SecretField`] (literal or `env://` ref) into the raw
/// secret string the adapter will use.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError>;
}

/// The substrate resolver: materialises literals and `env://` refs,
/// and fails closed on the decommissioned `vault://` scheme.
pub struct LiteralResolver;

#[async_trait]
impl SecretResolver for LiteralResolver {
    async fn resolve(&self, field: &SecretField) -> Result<String, ResolveError> {
        match field {
            SecretField::Literal(s) => Ok(s.clone()),
            SecretField::Env { var } => resolve_env(var),
            SecretField::Vault { path, field } => Err(ResolveError::VaultDecommissioned {
                ref_string: format!("vault://{path}#{field}"),
            }),
        }
    }
}

/// Materialise an `env://<VARNAME>` ref from the process environment.
/// This is how the Vault-less substrate delivers secrets (GCP Secret
/// Manager → kamal `.kamal/secrets` → container env). A missing or
/// non-UTF-8 variable fails the boot closed (M-SECRETS-1).
fn resolve_env(var: &str) -> Result<String, ResolveError> {
    std::env::var(var).map_err(|_| ResolveError::EnvNotSet {
        var: var.to_string(),
    })
}

/// Errors the resolver can surface at boot. Every variant exits
/// the binary non-zero (M-SECRETS-1 / FR-L-4): the substrate must
/// see a misconfigured deploy fail closed.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(
        "manifest declares `{ref_string}` but Vault was decommissioned — \
         migrate the credential to an `env://<VARNAME>` ref seeded via GCP \
         Secret Manager → kamal `.kamal/secrets` (M-SECRETS-1, FR-L-6)"
    )]
    VaultDecommissioned { ref_string: String },
    #[error(
        "manifest declares `env://{var}` but the environment variable is unset or not UTF-8 — \
         the substrate must inject it (GCP Secret Manager → kamal `.kamal/secrets`)"
    )]
    EnvNotSet { var: String },
}

impl ResolveError {
    /// The declared `env://<VARNAME>` whose absence caused this error,
    /// or `None` for any other failure (Vault decommissioned, etc.).
    ///
    /// This is the precise signal the binary's optional-adapter opt-in
    /// keys off: only an `env://` ref that the substrate failed to inject
    /// is eligible to be skipped (`TRITON_OPTIONAL_ADAPTERS`). Every
    /// other resolution failure stays fatal.
    pub fn missing_env_var(&self) -> Option<&str> {
        match self {
            ResolveError::EnvNotSet { var } => Some(var),
            ResolveError::VaultDecommissioned { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LiteralResolver, ResolveError, SecretResolver};
    use triton_manifest::SecretField;

    // A unique var name keeps this hermetic under the parallel test
    // runner (process-global env).
    const VAR: &str = "TRITON_SECRETS_TEST_ENV_REF_92F1";

    #[tokio::test]
    async fn literal_resolver_materialises_env_refs() {
        // env:// resolves with no backend — the substrate path.
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

    #[tokio::test]
    async fn vault_refs_fail_closed_post_decommission() {
        let err = LiteralResolver
            .resolve(&SecretField::Vault {
                path: "kv/data/apps/dz/triton/nonprod/telegram".into(),
                field: "bot_token".into(),
            })
            .await
            .expect_err("vault:// must fail closed after decommission");
        assert!(
            matches!(err, ResolveError::VaultDecommissioned { .. }),
            "{err:?}"
        );
    }
}
