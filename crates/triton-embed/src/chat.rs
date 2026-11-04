//! Chat-channel wiring shared by the `triton` binary and a host that embeds
//! Triton in-process (feature `chat`).
//!
//! [`msteams`] builds one Microsoft Teams adapter from the manifest with the
//! binary's NFR-S-4 egress checks, attaches the task runner (long-running
//! turns live in the same store as A2A's and MCP's) and resumes the stored
//! ones. The host merges the returned router and, when it serves proactive
//! sends, registers the adapter as an outbound courier.

use std::sync::Arc;

use axum::Router;
use triton_adapters_http::tasks::Runner;
use triton_chat_msteams::MsTeamsAdapter;
use triton_core::Dispatcher;
use triton_manifest::Manifest;
use triton_secrets::LiteralResolver;

/// The Teams adapter's process-level settings (the binary's
/// `TRITON_MSTEAMS_*` variables). Every override is refused outside
/// `env=local`.
#[derive(Debug, Clone)]
pub struct MsTeamsOpts {
    /// Bot Framework OpenID discovery (`TRITON_MSTEAMS_OPENID_URL`); must
    /// be the canonical Microsoft endpoint outside `local`.
    pub openid_url: String,
    /// Token endpoint override (`TRITON_MSTEAMS_TOKEN_URL`); `local` only.
    pub token_url: Option<String>,
    /// Extra `serviceUrl` hosts (`TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS`);
    /// `local` only.
    pub extra_service_url_hosts: Vec<String>,
    /// Ack the webhook at once and reply out of band
    /// (`TRITON_MSTEAMS_ASYNC`).
    pub async_reply: bool,
    /// Another adapter of this process already serves the canonical
    /// `/api/messages` path (triton#247); a second claimant is refused,
    /// because `Router::merge` would panic on the overlap.
    pub canonical_path_taken: bool,
}

impl Default for MsTeamsOpts {
    fn default() -> Self {
        Self {
            openid_url: triton_chat_msteams::jwt_verifier::DEFAULT_OPENID_URL.to_string(),
            token_url: None,
            extra_service_url_hosts: Vec::new(),
            async_reply: false,
            canonical_path_taken: false,
        }
    }
}

impl MsTeamsOpts {
    /// Read the `TRITON_MSTEAMS_*` variables with the binary's defaults and
    /// parsing. An unset or empty variable keeps the default.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        let d = Self::default();
        Self {
            openid_url: var("TRITON_MSTEAMS_OPENID_URL")
                .filter(|v| !v.is_empty())
                .unwrap_or(d.openid_url),
            token_url: var("TRITON_MSTEAMS_TOKEN_URL").filter(|v| !v.is_empty()),
            extra_service_url_hosts: var("TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            async_reply: matches!(
                var("TRITON_MSTEAMS_ASYNC")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "true" | "1"
            ),
            canonical_path_taken: false,
        }
    }
}

/// Why [`msteams`] did not build the adapter.
#[derive(Debug)]
pub enum MsTeamsError {
    /// An NFR-S-4 egress rule refused the settings; always fatal.
    Egress(String),
    /// The adapter claims `/api/messages` and another one already does.
    CanonicalPathTaken(String),
    /// The manifest has no adapter of that name.
    UnknownAdapter(String),
    /// The adapter itself refused (secrets, identity, kind). The binary
    /// lets an optional adapter be skipped on this one.
    Build(triton_chat_msteams::BuildError),
}

impl std::fmt::Display for MsTeamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Egress(m) => write!(f, "msteams egress (NFR-S-4): {m}"),
            Self::CanonicalPathTaken(name) => write!(
                f,
                "adapter `{name}`: at most one adapter may set `inbound.canonical_path` \
                 (triton#247)"
            ),
            Self::UnknownAdapter(name) => write!(f, "the manifest has no adapter `{name}`"),
            Self::Build(e) => write!(f, "msteams adapter: {e}"),
        }
    }
}

impl std::error::Error for MsTeamsError {}

fn is_canonical_url(raw: &str, expected_scheme: &str, expected_host: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    parsed.scheme() == expected_scheme && parsed.host_str() == Some(expected_host)
}

/// The NFR-S-4 rules for the Teams settings: outside `local` the OpenID
/// URL is the canonical Microsoft endpoint and nothing else is overridden.
fn check_egress(env: &str, opts: &MsTeamsOpts) -> Result<(), MsTeamsError> {
    if env == "local" {
        return Ok(());
    }
    // NFR-S-4: outside `local` env the OpenID discovery URL MUST be the
    // canonical Microsoft endpoint — no override that could redirect JWKS
    // fetches to an attacker-controlled host. The token endpoint is
    // hardcoded inside `token_client.rs` for the same reason; the
    // `serviceUrl` reply target rides inside a JWT just verified, so it is
    // trusted by derivation rather than allow-listed.
    const CANONICAL_OPENID: &str = triton_chat_msteams::jwt_verifier::DEFAULT_OPENID_URL;
    // PR 37: exact-string equality (the canonical URL has one documented
    // form) AND a parsed-URL host check, so a path-only tweak like
    // `.../openidconfiguration?foo=bar` still fails closed.
    if opts.openid_url != CANONICAL_OPENID
        || !is_canonical_url(&opts.openid_url, "https", "login.botframework.com")
    {
        tracing::error!(
            env = %env,
            msteams_openid_url = %opts.openid_url,
            "non-`local` env MUST use TRITON_MSTEAMS_OPENID_URL={CANONICAL_OPENID} (NFR-S-4 egress allowlist)",
        );
        return Err(MsTeamsError::Egress(format!(
            "TRITON_MSTEAMS_OPENID_URL must be {CANONICAL_OPENID} outside env=local"
        )));
    }
    // PR 37 critical finding: the token endpoint accepts
    // `client_credentials`; posting them at another host exfiltrates the
    // bot's secret. Only the integration fixture sets it.
    if opts.token_url.is_some() {
        tracing::error!(
            env = %env,
            msteams_token_url = %opts.token_url.as_deref().unwrap_or(""),
            "non-`local` env MUST NOT set TRITON_MSTEAMS_TOKEN_URL — \
             the Microsoft token endpoint is hardcoded for NFR-S-4 \
             (overriding would post client_credentials at an \
             arbitrary host)",
        );
        return Err(MsTeamsError::Egress(
            "TRITON_MSTEAMS_TOKEN_URL must not be set outside env=local".into(),
        ));
    }
    // PR 37: the `serviceUrl` host extras, likewise test-only.
    if !opts.extra_service_url_hosts.is_empty() {
        tracing::error!(
            env = %env,
            hosts = ?opts.extra_service_url_hosts,
            "non-`local` env MUST NOT set TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS \
             (NFR-S-4 egress allowlist for outbound reply Activity)",
        );
        return Err(MsTeamsError::Egress(
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS must not be set outside env=local".into(),
        ));
    }
    Ok(())
}

/// Build the Teams adapter `adapter_name` of `manifest`: NFR-S-4 checks,
/// the adapter itself (secrets resolve as literals or `env://` refs, as in
/// the binary), the canonical-path claim, then the task runner and the
/// resume of its stored tasks. Returns its webhook router
/// (`/<name>/webhook`, plus `/api/messages` when it claims it) and the
/// adapter, which is also the outbound courier for proactive sends.
pub async fn msteams(
    manifest: &Manifest,
    adapter_name: &str,
    env: &str,
    dispatcher: Arc<Dispatcher>,
    runner: Runner,
    opts: &MsTeamsOpts,
) -> Result<(Router, Arc<MsTeamsAdapter>), MsTeamsError> {
    check_egress(env, opts)?;
    let adapter = manifest
        .adapters
        .get(adapter_name)
        .ok_or_else(|| MsTeamsError::UnknownAdapter(adapter_name.to_string()))?;
    let overrides = triton_chat_msteams::AdapterOverrides {
        openid_url: Some(opts.openid_url.clone()),
        token_url: opts.token_url.clone(),
        extra_service_url_hosts: opts.extra_service_url_hosts.clone(),
    };
    let courier = triton_chat_msteams::CourierConfig {
        tracker: None,
        enabled: opts.async_reply,
    };
    let built = MsTeamsAdapter::from_manifest(
        adapter_name,
        adapter,
        &LiteralResolver,
        dispatcher,
        overrides,
        courier,
    )
    .await
    .map_err(MsTeamsError::Build)?;
    if built.canonical_path() && opts.canonical_path_taken {
        tracing::error!(
            adapter = %adapter_name,
            "at most one adapter may set `inbound.canonical_path`                                          (triton#247): the canonical /api/messages route is fixed                                          and a second claimant would panic Router::merge, taking                                          REST/MCP/A2A down with it",
        );
        return Err(MsTeamsError::CanonicalPathTaken(adapter_name.to_string()));
    }
    tracing::info!(adapter = %adapter_name, "msteams webhook adapter wired");
    // F8b: long-running Teams turns are tasks in the same store as A2A's
    // (persisted with TRITON_TASK_DB); stored ones resume at boot.
    let built = Arc::new(built.with_tasks(runner));
    built.resume_stored_tasks();
    let router = built.clone().router();
    Ok((router, built))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_refused_outside_local() {
        let ok = MsTeamsOpts::default();
        assert!(check_egress("prod", &ok).is_ok());
        let openid = MsTeamsOpts {
            openid_url: "https://login.botframework.com/v1/.well-known/openidconfiguration?x=1"
                .into(),
            ..MsTeamsOpts::default()
        };
        let token = MsTeamsOpts {
            token_url: Some("http://127.0.0.1:1/token".into()),
            ..MsTeamsOpts::default()
        };
        let hosts = MsTeamsOpts {
            extra_service_url_hosts: vec!["127.0.0.1".into()],
            ..MsTeamsOpts::default()
        };
        for (opts, var) in [
            (&openid, "TRITON_MSTEAMS_OPENID_URL"),
            (&token, "TRITON_MSTEAMS_TOKEN_URL"),
            (&hosts, "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS"),
        ] {
            match check_egress("prod", opts) {
                Err(MsTeamsError::Egress(m)) => assert!(m.contains(var), "{m}"),
                other => panic!("{var}: {other:?}"),
            }
            assert!(check_egress("local", opts).is_ok(), "{var} is fine locally");
        }
    }
}
