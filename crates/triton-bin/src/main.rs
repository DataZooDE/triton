//! `triton` — multi-protocol agent-ingress gateway. See `doc/`.
//!
//! Scope so far:
//! * PR 1 — workspace skeleton, REST `/healthz`.
//! * PR 2 — three listeners + graceful SIGTERM/SIGINT drain.
//! * PR 3 — 12-factor `Settings` + `GET /version`.
//! * PR 4 — dispatcher + audit emitter + in-process `echo` tool,
//!   driven through `POST /v1/tools/:name`.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use triton_adapters_http::a2a::{A2aState, InMemoryTaskStore};
use triton_adapters_http::identity::IdentityProvider;
use triton_adapters_http::mcp::{McpSessions, McpState};
use triton_adapters_http::rest::RestState;
use triton_chat_telegram::{CourierConfig, TelegramAdapter};
use triton_core::{Dispatcher, Metrics, RuntimeInfo, ToolRegistry, UpstreamDispatch};
use triton_identity::{OidcConfig, OidcVerifier};
use triton_manifest::{AdapterKind, InboundKind};
use triton_rasterizer::{Client as RasterizerClient, ClientConfig as RasterizerConfig};
use triton_secrets::{LiteralResolver, SecretResolver, VaultKvResolver};
use triton_upstream::{ConsulClient, UpstreamConfig, UpstreamRouter, VaultClient};

mod settings;
mod tools;
use settings::Settings;

const BINARY_SHA: &str = env!("TRITON_BUILD_SHA");

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();
    let settings = Arc::new(Settings::from_args());

    // v0.2 manifest, if configured. Loaded BEFORE we bind any
    // listeners — a malformed `adapter.yaml` must cause a clean
    // non-zero exit so Nomad reschedules rather than serving with
    // a broken config (FR-L-4..6, M-MANIFEST-1 / M-COVERAGE-1 /
    // M-SECRETS-1).
    let manifest: Option<triton_manifest::Manifest> = if let Some(path) = &settings.manifest_path {
        let manifest_env = match settings.env.as_str() {
            "local" | "" => triton_manifest::Env::Dev,
            _ => triton_manifest::Env::Production,
        };
        match triton_manifest::Manifest::load(path) {
            Ok(m) => match m.validate(manifest_env) {
                Ok(warnings) => {
                    tracing::info!(
                        path = %path.display(),
                        adapters = m.adapters.len(),
                        tools = m.tools.len(),
                        warnings = warnings.len(),
                        "adapter.yaml loaded",
                    );
                    for w in warnings {
                        tracing::warn!("manifest: {w}");
                    }
                    Some(m)
                }
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "manifest validation failed");
                    std::process::exit(2);
                }
            },
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "manifest load failed");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mcp_addr: SocketAddr = (settings.host, settings.mcp_port).into();
    let a2a_addr: SocketAddr = (settings.host, settings.a2a_port).into();
    let rest_addr: SocketAddr = (settings.host, settings.rest_port).into();

    let mcp_listener = TcpListener::bind(mcp_addr).await?;
    let a2a_listener = TcpListener::bind(a2a_addr).await?;
    let rest_listener = TcpListener::bind(rest_addr).await?;
    // FR-O-3 / G-7: /metrics binds on the tailnet interface,
    // never on the public REST port. `port = 0` disables it.
    let metrics_listener = if settings.metrics_port != 0 {
        let addr: SocketAddr = (settings.metrics_host, settings.metrics_port).into();
        let l = TcpListener::bind(addr).await?;
        Some((addr, l))
    } else {
        None
    };
    tracing::info!(
        mcp = %mcp_addr,
        a2a = %a2a_addr,
        rest = %rest_addr,
        metrics = ?metrics_listener.as_ref().map(|(a, _)| a),
        env = %settings.env,
        binary_sha = BINARY_SHA,
        drain_deadline_secs = settings.drain_deadline.as_secs(),
        "triton: listeners bound",
    );

    let shutdown = CancellationToken::new();
    let signal_task = {
        let token = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv()    => tracing::info!("SIGTERM received, draining"),
                _ = sigint.recv()     => tracing::info!("SIGINT received, draining"),
                _ = token.cancelled() => {}
            }
            token.cancel();
        })
    };

    let runtime = Arc::new(RuntimeInfo {
        binary_sha: BINARY_SHA.to_string(),
        image_sha: settings.image_sha.clone(),
        env: settings.env.clone(),
        package_version: env!("CARGO_PKG_VERSION").to_string(),
    });
    let discovery = Arc::new(triton_adapters_http::rest::RuntimeDiscovery {
        env: settings.env.clone(),
        image_sha: settings.image_sha.clone(),
        package_version: env!("CARGO_PKG_VERSION").to_string(),
        binary_sha: BINARY_SHA.to_string(),
        oidc_issuer: settings.oidc_issuer.clone(),
        oidc_audience: settings.oidc_audience.clone(),
        oidc_client_id: settings.explorer_client_id.clone(),
    });

    let metrics = Arc::new(Metrics::new());
    let registry = Arc::new(build_registry());
    let mut dispatcher =
        Dispatcher::new(registry, settings.env.clone()).with_metrics(metrics.clone());

    // Wire the upstream router if Consul + Vault are configured.
    //
    // Partial-wiring rules (PR 16 widened the original PR 9 rule to
    // let Vault be configured without Consul, so the secret
    // resolver can boot in deploys that don't need upstream
    // routing — e.g. a chat-only Telegram gateway):
    //   * consul + vault_url + vault_token  → upstream router on
    //   * vault_url + vault_token, no consul → resolver only, no router
    //   * vault_url without vault_token, or vault_token without
    //     vault_url → fatal (auth gap)
    //   * consul without vault_url+vault_token → fatal (router
    //     can't mint per-call OIDC tokens without Vault)
    //   * none → in-process tools only
    match (
        &settings.consul_url,
        &settings.vault_url,
        &settings.vault_token,
    ) {
        (Some(consul), Some(vault), Some(token)) => {
            let router = UpstreamRouter::new(
                ConsulClient::new(consul.clone()),
                VaultClient::new(vault.clone(), token.clone()),
                UpstreamConfig {
                    circuit_open_after: settings.circuit_open_after,
                    circuit_cooldown: settings.circuit_cooldown,
                    upstream_timeout: settings.upstream_timeout,
                    vault_role: settings.vault_oidc_role.clone(),
                    env_label: settings.env.clone(),
                },
            );
            tracing::info!(consul, vault, "upstream router enabled");
            dispatcher = dispatcher.with_upstream(Arc::new(router) as Arc<dyn UpstreamDispatch>);
        }
        (None, Some(vault), Some(_)) => {
            tracing::info!(
                vault,
                "vault configured without consul; secret resolver enabled, upstream router off",
            );
        }
        (None, None, None) => {
            tracing::warn!(
                "no upstream router configured; dispatcher serves in-process tools only"
            );
        }
        (Some(_), None, _) | (Some(_), _, None) => {
            tracing::error!(
                "TRITON_CONSUL_URL requires TRITON_VAULT_URL and TRITON_VAULT_TOKEN (upstream router needs Vault for per-call OIDC swap)"
            );
            std::process::exit(2);
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            tracing::error!("TRITON_VAULT_URL and TRITON_VAULT_TOKEN must be set together");
            std::process::exit(2);
        }
    }
    let dispatcher = Arc::new(dispatcher);

    // Build the OIDC verifier if the substrate injected an issuer.
    // When unset we fall back to the cfg-gated dev-token path.
    let identity = Arc::new(IdentityProvider::new(
        match (&settings.oidc_issuer, &settings.oidc_audience) {
            (Some(issuer), Some(aud)) => {
                tracing::info!(issuer, aud, "OIDC verifier enabled");
                Some(Arc::new(OidcVerifier::new(OidcConfig::new(
                    issuer.clone(),
                    aud.clone(),
                ))))
            }
            (Some(_), None) | (None, Some(_)) => {
                tracing::error!("TRITON_OIDC_ISSUER and TRITON_OIDC_AUDIENCE must be set together");
                std::process::exit(2);
            }
            (None, None) => {
                #[cfg(feature = "dev-token")]
                tracing::warn!(
                    "no OIDC issuer configured; dev-token fallback in effect (dev builds only)"
                );
                #[cfg(not(feature = "dev-token"))]
                tracing::error!(
                    "no OIDC issuer configured and dev-token disabled at build time; \
                     every bearer will be rejected"
                );
                None
            }
        },
    ));

    let manifest_arc = manifest.as_ref().map(|m| Arc::new(m.clone()));
    let rest_state = RestState {
        runtime: runtime.clone(),
        discovery: discovery.clone(),
        dispatcher: dispatcher.clone(),
        identity: identity.clone(),
        manifest: manifest_arc,
        metrics: metrics.clone(),
    };
    let a2a_state = A2aState {
        dispatcher: dispatcher.clone(),
        tasks: InMemoryTaskStore::new(),
        identity: identity.clone(),
    };
    let mcp_state = McpState {
        dispatcher: dispatcher.clone(),
        sessions: McpSessions::new(),
        identity: identity.clone(),
    };

    // v0.2 chat-channel adapters. Each manifest-declared adapter
    // whose `inbound.kind == webhook` mounts its router on the
    // shared chat-webhook listener at `/<name>/webhook`. Boot fails
    // hard on any build error so misconfigured secrets never serve
    // traffic (M-SECRETS-1 / FR-L-4).
    //
    // The secret resolver is picked once for all adapters: Vault
    // KV v2 when the substrate injected `TRITON_VAULT_URL` +
    // `_TOKEN`, literal-only otherwise. PR 13's warn-and-skip on
    // Vault refs is gone — a Vault ref without a configured Vault
    // is now a fatal misconfiguration (Codex called this out as a
    // PR 13 concern).
    let resolver: Arc<dyn SecretResolver> = match (&settings.vault_url, &settings.vault_token) {
        (Some(url), Some(token)) => {
            tracing::info!(vault = %url, "secret resolver: vault kv v2");
            Arc::new(VaultKvResolver::new(url.clone(), token.clone()))
        }
        _ => {
            tracing::info!("secret resolver: literal-only (no TRITON_VAULT_URL configured)");
            Arc::new(LiteralResolver)
        }
    };

    let mut chat_router: Option<axum::Router> = None;
    // PR 34: Signal's I/O is a persistent socket, not an HTTP
    // webhook — its inbound runs as a tokio task spawned at boot,
    // not a router mount. Both shapes coexist in the same loop;
    // each adapter dispatches on `inbound.kind`.
    let mut socket_adapter_joins: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(m) = &manifest {
        for (name, adapter) in &m.adapters {
            if adapter.inbound.kind == InboundKind::Socket {
                match adapter.kind {
                    AdapterKind::Signal => {
                        // NFR-S-4 egress allowlist: outside `local`
                        // env the operator MUST set
                        // `TRITON_SIGNAL_SIGNALD_ADDR`, and the
                        // address MUST resolve to a tailnet target.
                        //
                        // PR 37 hardening: previously any non-empty
                        // string was accepted, which let a misset env
                        // var redirect signald connections at an
                        // arbitrary host. Now:
                        //   * empty → reject (must be set explicitly)
                        //   * `unix://...` → accept (file path, not a
                        //     network destination)
                        //   * `tcp://host[:port]` (or bare `host:port`)
                        //     → host MUST end with `.ts.net`, i.e. a
                        //     Tailscale tailnet hostname.
                        if settings.env != "local" {
                            let addr = settings.signal_signald_addr.as_str();
                            let allowed = if addr.is_empty() {
                                false
                            } else if is_unix_socket_addr(addr) {
                                true
                            } else {
                                // tcp:// or bare host:port. Parse the
                                // host portion and require a `.ts.net`
                                // suffix on a label boundary.
                                let host = parse_host(addr).unwrap_or("");
                                !host.is_empty()
                                    && host
                                        .strip_suffix(".ts.net")
                                        .is_some_and(|prefix| !prefix.is_empty())
                            };
                            if !allowed {
                                tracing::error!(
                                    env = %settings.env,
                                    signald_addr = %addr,
                                    "non-`local` env MUST set TRITON_SIGNAL_SIGNALD_ADDR \
                                     to a `unix://...` path or a `tcp://*.ts.net[:port]` \
                                     tailnet target (NFR-S-4 egress allowlist)",
                                );
                                std::process::exit(2);
                            }
                        }
                        // If the operator supplied an override, the
                        // adapter's manifest field gets shadowed.
                        // We do this by mutating a clone of the
                        // adapter — the manifest itself is
                        // immutable per FR-L-4.
                        let mut effective = adapter.clone();
                        if !settings.signal_signald_addr.is_empty() {
                            effective.inbound.credentials.insert(
                                "signald_addr".to_string(),
                                triton_manifest::SecretField::Literal(
                                    settings.signal_signald_addr.clone(),
                                ),
                            );
                        }
                        match triton_chat_signal::SignalAdapter::from_manifest(
                            name,
                            &effective,
                            resolver.as_ref(),
                            dispatcher.clone(),
                        )
                        .await
                        {
                            Ok(built) => {
                                tracing::info!(
                                    adapter = %name,
                                    "signal socket adapter wired",
                                );
                                let h = Arc::new(built).spawn(shutdown.clone());
                                socket_adapter_joins.push(h);
                            }
                            Err(e) => {
                                tracing::error!(
                                    adapter = %name,
                                    error = %e,
                                    "signal adapter build failed",
                                );
                                std::process::exit(2);
                            }
                        }
                    }
                    _ => {
                        tracing::warn!(
                            adapter = %name,
                            kind = ?adapter.kind,
                            "socket-inbound adapter kind not implemented yet; skipping",
                        );
                    }
                }
                continue;
            }
            if adapter.inbound.kind != InboundKind::Webhook {
                continue;
            }
            match adapter.kind {
                AdapterKind::Telegram => {
                    // NFR-S-4 v0.2: only `api.telegram.org` is on the
                    // substrate ACL allowlist. Outside `local` env we
                    // refuse any `TRITON_TELEGRAM_API_BASE` override
                    // that doesn't parse to exactly that host so a
                    // misconfigured deploy (or compromised env var)
                    // cannot exfiltrate user content + the bot token
                    // to an arbitrary host (PR 37 hardened the check
                    // from string-equality to parsed-URL equality so
                    // a trailing slash or path doesn't matter).
                    const CANONICAL: &str = "https://api.telegram.org";
                    if settings.env != "local"
                        && !is_canonical_url(
                            &settings.telegram_api_base,
                            "https",
                            "api.telegram.org",
                        )
                    {
                        tracing::error!(
                            env = %settings.env,
                            telegram_api_base = %settings.telegram_api_base,
                            "non-`local` env MUST use TRITON_TELEGRAM_API_BASE={CANONICAL} (NFR-S-4 egress allowlist)",
                        );
                        std::process::exit(2);
                    }
                    // PR 36 NFR-S-4: rasterizer is a network
                    // dependency in the adapter hot path. Default
                    // `http://127.0.0.1:9320` is fine for local
                    // dev; outside `local` env the operator MUST
                    // point at a tailnet-resolved hostname (any
                    // dotted host that isn't an IP literal — a
                    // `*.tailnet.ts.net` will do). Refusing
                    // loopback / public-by-default values stops a
                    // misconfigured deploy from exfiltrating
                    // dashboard payloads to an unaudited host. We
                    // run this check ONLY when a Telegram adapter
                    // is actually being wired — a Triton instance
                    // running without chat adapters has no
                    // rasterizer egress to allowlist.
                    if settings.env != "local" {
                        // PR 37: parse the URL through the `url` crate
                        // so a malformed env var fails closed at the
                        // parse step (the old hand-rolled `parse_host`
                        // accepted e.g. `not a url`). Same heuristics
                        // afterwards — anything that isn't a dotted
                        // hostname is treated as misconfigured.
                        let parsed_host: Option<String> = url::Url::parse(&settings.rasterizer_url)
                            .ok()
                            .and_then(|u| u.host_str().map(|h| h.to_string()));
                        let host_ok = parsed_host
                            .as_deref()
                            .map(|host| {
                                !host.is_empty()
                                    && host != "127.0.0.1"
                                    && host != "localhost"
                                    && host != "0.0.0.0"
                                    && host.contains('.')
                                    && !host.chars().all(|c| c.is_ascii_digit() || c == '.')
                            })
                            .unwrap_or(false);
                        if !host_ok {
                            tracing::error!(
                                env = %settings.env,
                                rasterizer_url = %settings.rasterizer_url,
                                "non-`local` env MUST set TRITON_RASTERIZER_URL to a tailnet-resolved hostname (NFR-S-4 egress allowlist)",
                            );
                            std::process::exit(2);
                        }
                    }
                    let courier_config = CourierConfig {
                        api_base: settings.telegram_api_base.clone(),
                        timeout: settings.courier_timeout,
                    };
                    // PR 36: build the rasterizer client lazily —
                    // dashboards are an opt-in feature, but the
                    // mapper already declares `dashboard:
                    // rasterised_png` in every manifest we ship, so
                    // the URL is essentially always set. Boot
                    // failure on a bad URL is fatal so dashboards
                    // can never silently fall through to text
                    // (M-RASTER-1).
                    let rasterizer = match RasterizerClient::new(RasterizerConfig {
                        base: settings.rasterizer_url.clone(),
                        ..Default::default()
                    }) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            tracing::error!(
                                rasterizer_url = %settings.rasterizer_url,
                                error = %e,
                                "rasterizer client build failed"
                            );
                            std::process::exit(2);
                        }
                    };
                    match TelegramAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                        courier_config,
                        rasterizer,
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "telegram webhook adapter wired");
                            let r = Arc::new(built).router();
                            chat_router = Some(match chat_router.take() {
                                Some(acc) => acc.merge(r),
                                None => r,
                            });
                        }
                        Err(e) => {
                            tracing::error!(adapter = %name, error = %e, "telegram adapter build failed");
                            std::process::exit(2);
                        }
                    }
                }
                AdapterKind::GoogleChat => {
                    // NFR-S-4: the only host on the substrate egress
                    // allowlist for the Google Chat adapter is the
                    // canonical googleapis.com JWKS endpoint. Outside
                    // `local` env we refuse any override of
                    // `TRITON_GOOGLE_CHAT_JWKS_URI` whose parsed host
                    // isn't `www.googleapis.com` exactly. PR 37
                    // replaced an earlier `starts_with` check that an
                    // attacker could bypass by registering
                    // `www.googleapis.com.evil.example` — the parsed
                    // host comparison is unambiguous.
                    const CANONICAL_JWKS_HOST: &str = "https://www.googleapis.com";
                    if settings.env != "local"
                        && !is_canonical_url(
                            &settings.google_chat_jwks_uri,
                            "https",
                            "www.googleapis.com",
                        )
                    {
                        tracing::error!(
                            env = %settings.env,
                            jwks_uri = %settings.google_chat_jwks_uri,
                            "non-`local` env MUST use TRITON_GOOGLE_CHAT_JWKS_URI with host www.googleapis.com (NFR-S-4 egress allowlist; canonical base {CANONICAL_JWKS_HOST})",
                        );
                        std::process::exit(2);
                    }
                    match triton_chat_googlechat::GoogleChatAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                        settings.google_chat_jwks_uri.clone(),
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "google_chat webhook adapter wired");
                            let r = Arc::new(built).router();
                            chat_router = Some(match chat_router.take() {
                                Some(acc) => acc.merge(r),
                                None => r,
                            });
                        }
                        Err(e) => {
                            tracing::error!(adapter = %name, error = %e, "google_chat adapter build failed");
                            std::process::exit(2);
                        }
                    }
                }
                AdapterKind::Discord => {
                    match triton_chat_discord::DiscordAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "discord interactions adapter wired");
                            let r = Arc::new(built).router();
                            chat_router = Some(match chat_router.take() {
                                Some(acc) => acc.merge(r),
                                None => r,
                            });
                        }
                        Err(e) => {
                            tracing::error!(adapter = %name, error = %e, "discord adapter build failed");
                            std::process::exit(2);
                        }
                    }
                }
                AdapterKind::WhatsappWeb => {
                    // NFR-S-4 v0.2: only `graph.facebook.com` is on
                    // the substrate ACL allowlist. Outside `local`
                    // env we refuse any `TRITON_WHATSAPP_API_BASE`
                    // whose parsed host isn't `graph.facebook.com`
                    // exactly. Mirrors the Telegram pattern; PR 37
                    // generalised this to a parsed-URL check so the
                    // operator can vary path / trailing-slash freely
                    // but cannot redirect to a sibling host.
                    const CANONICAL: &str = "https://graph.facebook.com";
                    if settings.env != "local"
                        && !is_canonical_url(
                            &settings.whatsapp_api_base,
                            "https",
                            "graph.facebook.com",
                        )
                    {
                        tracing::error!(
                            env = %settings.env,
                            whatsapp_api_base = %settings.whatsapp_api_base,
                            "non-`local` env MUST use TRITON_WHATSAPP_API_BASE with host graph.facebook.com (NFR-S-4 egress allowlist; canonical base {CANONICAL})",
                        );
                        std::process::exit(2);
                    }
                    let courier_config = triton_chat_whatsapp::CourierConfig {
                        api_base: settings.whatsapp_api_base.clone(),
                        timeout: settings.courier_timeout,
                    };
                    match triton_chat_whatsapp::WhatsAppAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                        courier_config,
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "whatsapp webhook adapter wired");
                            let r = Arc::new(built).router();
                            chat_router = Some(match chat_router.take() {
                                Some(acc) => acc.merge(r),
                                None => r,
                            });
                        }
                        Err(e) => {
                            tracing::error!(adapter = %name, error = %e, "whatsapp adapter build failed");
                            std::process::exit(2);
                        }
                    }
                }
                AdapterKind::MsTeams => {
                    // NFR-S-4: outside `local` env the OpenID discovery
                    // URL MUST be the canonical Microsoft endpoint —
                    // operators get no `TRITON_MSTEAMS_OPENID_URL`
                    // override path that could redirect JWKS fetches
                    // to an attacker-controlled host. The token
                    // endpoint is hardcoded inside `token_client.rs`
                    // for the same reason; the `serviceUrl` reply
                    // target rides inside a JWT we just verified, so
                    // it's trusted-by-derivation rather than allow-
                    // listed.
                    const CANONICAL_OPENID: &str =
                        triton_chat_msteams::jwt_verifier::DEFAULT_OPENID_URL;
                    // PR 37: keep exact-string equality (the canonical
                    // URL has a single documented form) AND add a
                    // parsed-URL host check so a path-only tweak like
                    // `.../openidconfiguration?foo=bar` still fails
                    // closed. Production has zero reason to vary this.
                    if settings.env != "local"
                        && (settings.msteams_openid_url != CANONICAL_OPENID
                            || !is_canonical_url(
                                &settings.msteams_openid_url,
                                "https",
                                "login.botframework.com",
                            ))
                    {
                        tracing::error!(
                            env = %settings.env,
                            msteams_openid_url = %settings.msteams_openid_url,
                            "non-`local` env MUST use TRITON_MSTEAMS_OPENID_URL={CANONICAL_OPENID} (NFR-S-4 egress allowlist)",
                        );
                        std::process::exit(2);
                    }
                    // PR 37 critical finding: the same NFR-S-4 rule
                    // applies to `TRITON_MSTEAMS_TOKEN_URL`. The token
                    // endpoint accepts `client_credentials` — posting
                    // those credentials at an attacker-controlled host
                    // exfiltrates the bot's `client_secret`. There is
                    // NO valid production reason to override; only
                    // the integration test fixture sets it. Refuse
                    // any value outside `local`.
                    if settings.env != "local" && settings.msteams_token_url.is_some() {
                        tracing::error!(
                            env = %settings.env,
                            msteams_token_url = %settings.msteams_token_url.as_deref().unwrap_or(""),
                            "non-`local` env MUST NOT set TRITON_MSTEAMS_TOKEN_URL — \
                             the Microsoft token endpoint is hardcoded for NFR-S-4 \
                             (overriding would post client_credentials at an \
                             arbitrary host)",
                        );
                        std::process::exit(2);
                    }
                    // PR 37: same NFR-S-4 rule for the `serviceUrl`
                    // host extras. Only the integration test fixture
                    // needs additional hosts; production must use
                    // Microsoft's documented suffixes only.
                    if settings.env != "local"
                        && !settings.msteams_extra_service_url_hosts.is_empty()
                    {
                        tracing::error!(
                            env = %settings.env,
                            hosts = ?settings.msteams_extra_service_url_hosts,
                            "non-`local` env MUST NOT set TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS \
                             (NFR-S-4 egress allowlist for outbound reply Activity)",
                        );
                        std::process::exit(2);
                    }
                    let overrides = triton_chat_msteams::AdapterOverrides {
                        openid_url: Some(settings.msteams_openid_url.clone()),
                        token_url: settings.msteams_token_url.clone(),
                        extra_service_url_hosts: settings.msteams_extra_service_url_hosts.clone(),
                    };
                    match triton_chat_msteams::MsTeamsAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                        overrides,
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "msteams webhook adapter wired");
                            let r = Arc::new(built).router();
                            chat_router = Some(match chat_router.take() {
                                Some(acc) => acc.merge(r),
                                None => r,
                            });
                        }
                        Err(e) => {
                            tracing::error!(adapter = %name, error = %e, "msteams adapter build failed");
                            std::process::exit(2);
                        }
                    }
                }
                _ => {
                    tracing::warn!(
                        adapter = %name,
                        kind = ?adapter.kind,
                        "adapter kind not implemented yet; skipping",
                    );
                }
            }
        }
    }

    let chat_listener = if chat_router.is_some() && settings.chat_webhook_port != 0 {
        let addr: SocketAddr = (settings.chat_webhook_host, settings.chat_webhook_port).into();
        let l = TcpListener::bind(addr).await?;
        tracing::info!(chat_webhook = %addr, "chat webhook listener bound");
        Some(l)
    } else {
        None
    };

    // Opt-in CORS layer for internal browser frontends (e.g. the
    // Flutter explorer at `apps/explorer/`). Mounted only when the
    // operator sets TRITON_CORS_ALLOWED_ORIGINS — empty list means
    // no layer is attached and headers stay identical to v0.1.
    let cors_layer = triton_adapters_http::cors::build_layer(&settings.cors_allowed_origins);
    if cors_layer.is_some() {
        tracing::info!(
            origins = ?settings.cors_allowed_origins,
            "CORS layer mounted on REST/MCP/A2A"
        );
    }
    let mcp_router = triton_adapters_http::mcp::router(mcp_state);
    let a2a_router = triton_adapters_http::a2a::router(a2a_state);
    let rest_router = triton_adapters_http::rest::router(rest_state);
    let mcp_router = match &cors_layer {
        Some(l) => mcp_router.layer(l.clone()),
        None => mcp_router,
    };
    let a2a_router = match &cors_layer {
        Some(l) => a2a_router.layer(l.clone()),
        None => a2a_router,
    };
    let rest_router = match &cors_layer {
        Some(l) => rest_router.layer(l.clone()),
        None => rest_router,
    };
    let serve_mcp = axum::serve(mcp_listener, mcp_router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_a2a = axum::serve(a2a_listener, a2a_router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_rest = axum::serve(rest_listener, rest_router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());

    // Optional fourth listener for tailnet-only `/metrics`. Lives
    // outside the public-routed listeners so Fabio cannot leak it
    // (G-7, NFR-S-2). When disabled, the future is a no-op that
    // returns Ok immediately on shutdown so the join shape is the
    // same regardless.
    let metrics_token = shutdown.clone();
    let metrics_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>,
    > = match metrics_listener {
        Some((_, l)) => {
            use std::future::IntoFuture;
            Box::pin(
                axum::serve(l, triton_adapters_http::metrics::router(metrics.clone()))
                    .with_graceful_shutdown(metrics_token.cancelled_owned())
                    .into_future(),
            )
        }
        None => Box::pin(async move {
            metrics_token.cancelled().await;
            Ok(())
        }),
    };

    // Optional chat-channel webhook listener. Same lifecycle as
    // /metrics — when disabled the future just waits for shutdown.
    let chat_token = shutdown.clone();
    let chat_fut: std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> =
        match (chat_listener, chat_router) {
            (Some(l), Some(router)) => {
                use std::future::IntoFuture;
                Box::pin(
                    axum::serve(l, router)
                        .with_graceful_shutdown(chat_token.cancelled_owned())
                        .into_future(),
                )
            }
            _ => Box::pin(async move {
                chat_token.cancelled().await;
                Ok(())
            }),
        };

    // PR 34: socket-inbound adapters (Signal) run as standalone
    // tokio tasks. Roll them up into one future so the drain
    // awaits them alongside the HTTP listeners. Each task self-
    // terminates when `shutdown` is cancelled.
    let socket_adapters_fut = async move {
        for j in socket_adapter_joins {
            let _ = j.await;
        }
        Ok::<(), std::io::Error>(())
    };
    let drain = async {
        let (a, b, c, m, ch, sa) = tokio::join!(
            serve_mcp,
            serve_a2a,
            serve_rest,
            metrics_fut,
            chat_fut,
            socket_adapters_fut,
        );
        a.and(b).and(c).and(m).and(ch).and(sa)
    };
    let outcome = tokio::time::timeout(settings.drain_deadline, drain).await;
    let serve_result = match outcome {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(
                deadline_secs = settings.drain_deadline.as_secs(),
                "drain deadline exceeded; in-flight connections aborted"
            );
            Ok(())
        }
    };

    shutdown.cancel();
    let _ = signal_task.await;

    let _ = std::io::stdout().flush();

    tracing::info!("triton: exit");
    serve_result
}

fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(tools::Echo));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::Delay));
    registry.register(Arc::new(tools::Narrate));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::DemoPanel));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::EmptySurface));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::FormOnlyDemo));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::FormOnlyDemoMulti));
    #[cfg(feature = "dev-token")]
    registry.register(Arc::new(tools::SubmittedForm));
    registry
}

/// Extract `host` from `scheme://host[:port][/path]`. Returns
/// None for unparseable URLs; the caller treats `None` as "not a
/// tailnet host" so a malformed env var fails closed.
fn parse_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // Strip path.
    let host_port = rest.split('/').next().unwrap_or(rest);
    // Strip port.
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() { None } else { Some(host) }
}

/// NFR-S-4: exact-host canonical-URL check. Returns true iff `raw`
/// parses as a URL whose scheme matches `expected_scheme` and whose
/// host matches `expected_host` byte-for-byte.
///
/// Why a parsed-URL check and not `starts_with`? A naive prefix
/// match like
/// `raw.starts_with("https://www.googleapis.com")` accepts
/// `https://www.googleapis.com.evil.example/path` — the attacker
/// just registers a subdomain whose label starts with
/// `www.googleapis.com`. Parsing extracts the authoritative host
/// component so the comparison is unambiguous.
fn is_canonical_url(raw: &str, expected_scheme: &str, expected_host: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    parsed.scheme() == expected_scheme && parsed.host_str() == Some(expected_host)
}

/// True iff `addr` looks like `unix://...` — Signal's signald
/// daemon supports both `tcp://` and `unix://` socket forms. The
/// NFR-S-4 tailnet check applies only to the `tcp://` form; a Unix
/// socket is a file path, not a network destination.
fn is_unix_socket_addr(addr: &str) -> bool {
    addr.starts_with("unix://")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .json()
        .init();
}

#[cfg(test)]
mod url_host_check_tests {
    use super::{is_canonical_url, is_unix_socket_addr};

    // NFR-S-4 / fix-up PR 37: the Google Chat / WhatsApp / MS Teams
    // egress-allowlist checks used `starts_with` which a sub-domain
    // attacker could trivially defeat (e.g.
    // `https://www.googleapis.com.evil.example/...`). The parsed-URL
    // helper closes that gap.

    #[test]
    fn canonical_googleapis_uri_accepts_only_canonical_host() {
        // Canonical: scheme + host match → true.
        assert!(is_canonical_url(
            "https://www.googleapis.com/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com",
            "https",
            "www.googleapis.com",
        ));
        // Subdomain-prefix attack — `starts_with` would accept.
        assert!(!is_canonical_url(
            "https://www.googleapis.com.evil.example/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com",
            "https",
            "www.googleapis.com",
        ));
        // Userinfo / @-host smuggling: `https://www.googleapis.com@evil.example/...`
        // — parser puts `evil.example` in host_str, so false.
        assert!(!is_canonical_url(
            "https://www.googleapis.com@evil.example/foo",
            "https",
            "www.googleapis.com",
        ));
        // Bare host (no subdomain).
        assert!(!is_canonical_url(
            "https://googleapis.com/foo",
            "https",
            "www.googleapis.com",
        ));
        // Wrong scheme.
        assert!(!is_canonical_url(
            "http://www.googleapis.com/foo",
            "https",
            "www.googleapis.com",
        ));
        // Unparseable junk.
        assert!(!is_canonical_url(
            "not a url",
            "https",
            "www.googleapis.com"
        ));
        assert!(!is_canonical_url("", "https", "www.googleapis.com"));
    }

    #[test]
    fn canonical_telegram_and_whatsapp_check() {
        assert!(is_canonical_url(
            "https://api.telegram.org",
            "https",
            "api.telegram.org"
        ));
        assert!(!is_canonical_url(
            "https://api.telegram.org.evil.example",
            "https",
            "api.telegram.org"
        ));
        assert!(is_canonical_url(
            "https://graph.facebook.com/v18.0/123/messages",
            "https",
            "graph.facebook.com"
        ));
        assert!(!is_canonical_url(
            "https://graph.facebook.com.evil.example",
            "https",
            "graph.facebook.com"
        ));
    }

    #[test]
    fn unix_socket_addr_recognised() {
        assert!(is_unix_socket_addr("unix:///var/run/signald/signald.sock"));
        assert!(!is_unix_socket_addr("tcp://signald.tailnet.ts.net:15432"));
        assert!(!is_unix_socket_addr("signald.tailnet.ts.net:15432"));
        assert!(!is_unix_socket_addr(""));
    }
}
