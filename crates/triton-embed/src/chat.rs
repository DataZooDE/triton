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
use clap::Parser;
use triton_adapters_http::tasks::Runner;
use triton_chat_msteams::MsTeamsAdapter;
use triton_core::Dispatcher;
use triton_manifest::{InboundKind, Manifest};
/// The resolvers a host passes to [`msteams`]: literals and `env://` refs
/// (what the binary uses), or its own [`SecretResolver`].
pub use triton_secrets::{LiteralResolver, SecretResolver};

/// The Teams adapter's process-level settings: the binary's
/// `TRITON_MSTEAMS_*` clap group (the binary flattens it into its CLI), so a
/// host and `triton` parse them identically. Every override is refused
/// outside `env=local`, an empty one included.
#[derive(Debug, Clone, Parser)]
pub struct MsTeamsConfig {
    /// OpenID discovery URL for the Microsoft Teams adapter. Default
    /// is the canonical Bot Framework endpoint. Outside `local` env
    /// the binary refuses any override (NFR-S-4 egress allowlist).
    #[arg(
        long,
        env = "TRITON_MSTEAMS_OPENID_URL",
        default_value = "https://login.botframework.com/v1/.well-known/openidconfiguration"
    )]
    pub msteams_openid_url: String,

    /// Optional override for Microsoft's OAuth2 token endpoint.
    /// `None` (the default) ⇒ the canonical hardcoded URL inside
    /// `token_client.rs`. Tests set this so the in-repo
    /// `FakeBotFramework` can mint stub access tokens.
    #[arg(long, env = "TRITON_MSTEAMS_TOKEN_URL")]
    pub msteams_token_url: Option<String>,

    /// PR 37: comma-separated `serviceUrl` host extras the MS Teams
    /// JWT verifier accepts. Empty by default; the binary refuses
    /// non-empty values outside `local` env.
    #[arg(
        long,
        env = "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS",
        default_value = "",
        value_parser = parse_hosts
    )]
    pub msteams_extra_service_url_hosts: ServiceUrlHosts,

    /// #635 P4: async reply courier for msteams — ack the webhook 200
    /// immediately and deliver out-of-band (streaming in 1:1 chats).
    /// Bot Framework abandons the inbound connection at ~15s, which on
    /// the inline path cancels the dispatch mid-turn. Accepts
    /// `true`/`1` (case-insensitive) as on, like the googlechat twin.
    #[arg(
        long,
        env = "TRITON_MSTEAMS_ASYNC",
        default_value = "false",
        action = clap::ArgAction::Set,
        value_parser = parse_on
    )]
    pub msteams_async: bool,
}

/// The `serviceUrl` host extras, parsed from a comma-separated list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceUrlHosts(pub Vec<String>);

fn parse_hosts(raw: &str) -> Result<ServiceUrlHosts, std::convert::Infallible> {
    Ok(ServiceUrlHosts(
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    ))
}

fn parse_on(raw: &str) -> Result<bool, std::convert::Infallible> {
    Ok(matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1"
    ))
}

impl MsTeamsConfig {
    /// Read the `TRITON_MSTEAMS_*` variables from the process environment
    /// with the binary's defaults and parsing. Command-line arguments are
    /// not read: the host owns its own CLI.
    pub fn from_env() -> Result<Self, crate::ingress::BootError> {
        Self::try_parse_from(["triton"])
            .map_err(|e| crate::ingress::BootError(format!("msteams: {e}")))
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
    /// The adapter's inbound is not a webhook: there is no route to mount.
    NotWebhook(String),
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
            Self::NotWebhook(name) => write!(
                f,
                "adapter `{name}`: a Teams adapter is mounted only with `inbound.kind: webhook`"
            ),
            Self::Build(e) => write!(f, "msteams adapter: {e}"),
        }
    }
}

impl std::error::Error for MsTeamsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Build(e) => Some(e),
            _ => None,
        }
    }
}

fn is_canonical_url(raw: &str, expected_scheme: &str, expected_host: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    parsed.scheme() == expected_scheme && parsed.host_str() == Some(expected_host)
}

/// The NFR-S-4 rules for the Teams settings: outside `local` the OpenID
/// URL is the canonical Microsoft endpoint and nothing else is overridden.
fn check_egress(env: &str, opts: &MsTeamsConfig) -> Result<(), MsTeamsError> {
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
    if opts.msteams_openid_url != CANONICAL_OPENID
        || !is_canonical_url(&opts.msteams_openid_url, "https", "login.botframework.com")
    {
        tracing::error!(
            env = %env,
            msteams_openid_url = %opts.msteams_openid_url,
            "non-`local` env MUST use TRITON_MSTEAMS_OPENID_URL={CANONICAL_OPENID} (NFR-S-4 egress allowlist)",
        );
        return Err(MsTeamsError::Egress(format!(
            "TRITON_MSTEAMS_OPENID_URL must be {CANONICAL_OPENID} outside env=local"
        )));
    }
    // PR 37 critical finding: the token endpoint accepts
    // `client_credentials`; posting them at another host exfiltrates the
    // bot's secret. Only the integration fixture sets it.
    if opts.msteams_token_url.is_some() {
        tracing::error!(
            env = %env,
            msteams_token_url = %opts.msteams_token_url.as_deref().unwrap_or(""),
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
    if !opts.msteams_extra_service_url_hosts.0.is_empty() {
        tracing::error!(
            env = %env,
            hosts = ?opts.msteams_extra_service_url_hosts.0,
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
/// the adapter itself (its secrets through `resolver`; the binary passes
/// [`LiteralResolver`], literals and `env://` refs), the canonical-path
/// claim, then the task runner and the resume of its stored tasks. Returns
/// its webhook router (`/<name>/webhook`, plus `/api/messages` when it
/// claims it) and the adapter, which is also the outbound courier for
/// proactive sends.
///
/// `canonical_path_claimed` is the process's one `/api/messages` claim
/// (triton#247): pass the same flag for every adapter of a router. A
/// second claimant is refused before its stored tasks resume, because
/// `Router::merge` would panic on the overlap.
#[allow(clippy::too_many_arguments)]
pub async fn msteams(
    manifest: &Manifest,
    adapter_name: &str,
    env: &str,
    dispatcher: Arc<Dispatcher>,
    runner: Runner,
    cfg: &MsTeamsConfig,
    resolver: &dyn SecretResolver,
    canonical_path_claimed: &mut bool,
) -> Result<(Router, Arc<MsTeamsAdapter>), MsTeamsError> {
    check_egress(env, cfg)?;
    let adapter = manifest
        .adapters
        .get(adapter_name)
        .ok_or_else(|| MsTeamsError::UnknownAdapter(adapter_name.to_string()))?;
    if adapter.inbound.kind != InboundKind::Webhook {
        return Err(MsTeamsError::NotWebhook(adapter_name.to_string()));
    }
    let overrides = triton_chat_msteams::AdapterOverrides {
        openid_url: Some(cfg.msteams_openid_url.clone()),
        token_url: cfg.msteams_token_url.clone(),
        extra_service_url_hosts: cfg.msteams_extra_service_url_hosts.0.clone(),
    };
    let courier = triton_chat_msteams::CourierConfig {
        tracker: None,
        enabled: cfg.msteams_async,
    };
    let built = MsTeamsAdapter::from_manifest(
        adapter_name,
        adapter,
        resolver,
        dispatcher,
        overrides,
        courier,
    )
    .await
    .map_err(MsTeamsError::Build)?;
    if built.canonical_path() {
        if *canonical_path_claimed {
            tracing::error!(
                adapter = %adapter_name,
                "at most one adapter may set `inbound.canonical_path` (triton#247): the \
                 canonical /api/messages route is fixed and a second claimant would panic \
                 Router::merge, taking REST/MCP/A2A down with it",
            );
            return Err(MsTeamsError::CanonicalPathTaken(adapter_name.to_string()));
        }
        *canonical_path_claimed = true;
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

    fn cfg(flags: &[&str]) -> MsTeamsConfig {
        let mut argv = vec!["t"];
        argv.extend_from_slice(flags);
        MsTeamsConfig::try_parse_from(argv).unwrap()
    }

    #[test]
    fn overrides_are_refused_outside_local() {
        assert!(check_egress("prod", &cfg(&[])).is_ok());
        let openid = cfg(&[
            "--msteams-openid-url",
            "https://login.botframework.com/v1/.well-known/openidconfiguration?x=1",
        ]);
        let token = cfg(&["--msteams-token-url", "http://127.0.0.1:1/token"]);
        let hosts = cfg(&["--msteams-extra-service-url-hosts", "127.0.0.1"]);
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
