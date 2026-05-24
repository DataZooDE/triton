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

    let rest_state = RestState {
        runtime: runtime.clone(),
        discovery: discovery.clone(),
        dispatcher: dispatcher.clone(),
        identity: identity.clone(),
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
    if let Some(m) = &manifest {
        for (name, adapter) in &m.adapters {
            if adapter.inbound.kind != InboundKind::Webhook {
                continue;
            }
            match adapter.kind {
                AdapterKind::Telegram => {
                    // NFR-S-4 v0.2: only `api.telegram.org` is on the
                    // substrate ACL allowlist. Outside `local` env we
                    // refuse any `TRITON_TELEGRAM_API_BASE` override
                    // so a misconfigured deploy (or compromised env
                    // var) cannot exfiltrate user content + the bot
                    // token to an arbitrary host. Codex flagged this
                    // in PR 18 review.
                    const CANONICAL: &str = "https://api.telegram.org";
                    if settings.env != "local" && settings.telegram_api_base != CANONICAL {
                        tracing::error!(
                            env = %settings.env,
                            telegram_api_base = %settings.telegram_api_base,
                            "non-`local` env MUST use TRITON_TELEGRAM_API_BASE={CANONICAL} (NFR-S-4 egress allowlist)",
                        );
                        std::process::exit(2);
                    }
                    let courier_config = CourierConfig {
                        api_base: settings.telegram_api_base.clone(),
                        timeout: settings.courier_timeout,
                    };
                    match TelegramAdapter::from_manifest(
                        name,
                        adapter,
                        resolver.as_ref(),
                        dispatcher.clone(),
                        courier_config,
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

    let drain = async {
        let (a, b, c, m, ch) =
            tokio::join!(serve_mcp, serve_a2a, serve_rest, metrics_fut, chat_fut);
        a.and(b).and(c).and(m).and(ch)
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
    registry
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
