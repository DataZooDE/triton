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
use triton_secrets::{LiteralResolver, SecretResolver};

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

    let rest_listener = TcpListener::bind(rest_addr).await?;
    // Single-port mode nests MCP at `/mcp` and A2A at `/a2a` on the REST
    // listener, so their own ports are never bound. In the three-port
    // default each gets its own listener.
    let (mcp_listener, a2a_listener) = if settings.single_port {
        (None, None)
    } else {
        let mcp = TcpListener::bind(mcp_addr).await?;
        let a2a = TcpListener::bind(a2a_addr).await?;
        (Some(mcp), Some(a2a))
    };
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
        // In single-port mode MCP/A2A are nested on the REST listener, so
        // only the REST addr is bound; report it for both via `?`.
        mcp = ?mcp_listener.as_ref().map(|_| mcp_addr),
        a2a = ?a2a_listener.as_ref().map(|_| a2a_addr),
        rest = %rest_addr,
        single_port = settings.single_port,
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
        // Three-port default: MCP/A2A live on their own ports and the SPA
        // uses its dev port-swap, so both bases are None. Single-port mode
        // nests the trio under these paths (like triton-embed), so the SPA
        // reaches the whole trio same-origin.
        mcp_base: settings.single_port.then(|| "/mcp".to_string()),
        a2a_base: settings.single_port.then(|| "/a2a".to_string()),
    });

    let metrics = Arc::new(Metrics::new());
    // Tool names claimed by `TRITON_STATIC_UPSTREAMS`. The dispatcher
    // prefers in-process tools, so registering one under a name the
    // static map claims would silently shadow the upstream agent and
    // make the mapped endpoint unreachable; those names are skipped at
    // registration.
    let static_upstream_tools: std::collections::HashSet<String> =
        std::env::var("TRITON_STATIC_UPSTREAMS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|spec| {
                spec.split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, _)| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    let registry = Arc::new(build_registry(&static_upstream_tools));
    let mut dispatcher =
        Dispatcher::new(registry, settings.env.clone()).with_metrics(metrics.clone());

    // Static-upstream OIDC signer: when a signing key + issuer + JWKS are all
    // configured, Triton mints a per-call RS256 JWT to agents (workload→workload
    // auth WITHOUT Vault) and serves discovery/JWKS (rest.rs). Shared (Arc) with
    // RestState below so the same key signs and is served. All-or-nothing.
    let static_signer: Option<Arc<triton_identity::JwtSigner>> = match (
        &settings.jwt_signing_key,
        &settings.static_upstream_issuer,
        &settings.jwt_jwks,
    ) {
        (Some(key), Some(issuer), Some(jwks_str)) => {
            let kid = settings
                .jwt_kid
                .clone()
                .unwrap_or_else(|| "triton-static".to_string());
            let jwks: serde_json::Value = serde_json::from_str(jwks_str).unwrap_or_else(|e| {
                tracing::error!(error = %e, "TRITON_JWT_JWKS is not valid JSON");
                std::process::exit(2);
            });
            // The signing key is accepted either as a raw PEM or base64-encoded
            // PEM. Multi-line PEM can't ride a single env var / Kamal env-file
            // line, so deployments base64 it; raw PEM still works for a file or
            // local dev. Detect by the PEM header after an optional decode.
            let pem_bytes: Vec<u8> = if key.trim_start().starts_with("-----BEGIN") {
                key.as_bytes().to_vec()
            } else {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(key.trim().as_bytes())
                    .unwrap_or_else(|e| {
                        tracing::error!(error = %e, "TRITON_JWT_SIGNING_KEY is neither PEM nor valid base64");
                        std::process::exit(2);
                    })
            };
            match triton_identity::JwtSigner::from_rsa_pem(&pem_bytes, kid, issuer.clone(), jwks) {
                Ok(s) => {
                    tracing::info!(issuer, "static-upstream OIDC signer enabled (RS256 + JWKS)");
                    Some(Arc::new(s))
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to build static-upstream OIDC signer");
                    std::process::exit(2);
                }
            }
        }
        (None, None, None) => None,
        _ => {
            tracing::error!(
                "static-upstream signing requires TRITON_JWT_SIGNING_KEY + TRITON_SELF_ISSUER + TRITON_JWT_JWKS together"
            );
            std::process::exit(2);
        }
    };

    // Wire the upstream dispatcher from the static `TRITON_STATIC_UPSTREAMS`
    // map (`name=host:port,...`). Consul discovery + the Vault per-call
    // OIDC swap were removed with the move off the HashiCorp stack to
    // Kamal; this static map is the only discovery mechanism. Unset →
    // the dispatcher serves in-process tools only.
    match std::env::var("TRITON_STATIC_UPSTREAMS")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(spec) => {
            let token = std::env::var("TRITON_STATIC_UPSTREAM_TOKEN")
                .unwrap_or_else(|_| "dev-token".to_string());
            let su =
                triton_upstream::StaticUpstream::from_spec(&spec, token, settings.upstream_timeout);
            // When a signer is configured, mint per-call RS256 JWTs
            // (production agents verify these via Triton's JWKS) instead
            // of the static dev-token bearer — workload→workload auth
            // without Vault.
            let su = if let Some(signer) = &static_signer {
                let aud = settings
                    .static_upstream_aud
                    .clone()
                    .unwrap_or_else(|| format!("agents-{}", settings.env));
                let tenant = settings.static_upstream_tenant.clone().unwrap_or_default();
                let forward_principal = settings.static_upstream_forward_principal;
                // #114: empty allowlist → None (caps only); else the set.
                let scope_allowlist: Option<std::collections::HashSet<String>> =
                    if settings.static_upstream_scope_allowlist.is_empty() {
                        None
                    } else {
                        Some(
                            settings
                                .static_upstream_scope_allowlist
                                .iter()
                                .cloned()
                                .collect(),
                        )
                    };
                tracing::info!(
                    spec = %spec, aud = %aud, tenant = %tenant, forward_principal,
                    scope_allowlist = ?settings.static_upstream_scope_allowlist,
                    "static upstream: dispatching by name with signed RS256 JWTs"
                );
                su.with_signer(
                    signer.clone(),
                    aud,
                    tenant,
                    forward_principal,
                    scope_allowlist,
                )
            } else {
                tracing::warn!(
                    spec = %spec,
                    "static upstream (dev): dispatching by name with the static bearer"
                );
                su
            };
            dispatcher = dispatcher.with_upstream(Arc::new(su) as Arc<dyn UpstreamDispatch>);
        }
        None => {
            tracing::warn!("no upstream configured; dispatcher serves in-process tools only");
        }
    }
    let dispatcher = Arc::new(dispatcher);

    // Build the OIDC verifier if the substrate injected an issuer.
    // When unset we fall back to the cfg-gated dev-token path.
    let oidc_verifier = match (&settings.oidc_issuer, &settings.oidc_audience) {
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
    };
    // Issue #67 — only honour X-Forwarded-Email when both the
    // operator-explicit flag is set AND OIDC is OFF (real Bearer/
    // PKCE end-to-end is the source of truth when OIDC is live).
    if settings.trust_forwarded_auth && oidc_verifier.is_some() {
        tracing::warn!(
            "TRITON_TRUST_FORWARDED_AUTH=true but OIDC is also configured — \
             forwarded-auth fast-path is DISABLED; OIDC Bearer wins (ADR-10)."
        );
    } else if settings.trust_forwarded_auth {
        tracing::warn!(
            "TRITON_TRUST_FORWARDED_AUTH=true — admitting requests carrying \
             X-Forwarded-Email from a co-located oauth2-proxy sidecar. \
             ONLY safe when Triton binds loopback (issue #67)."
        );
    }
    let identity = Arc::new(IdentityProvider::with_forwarded_auth(
        oidc_verifier,
        settings.trust_forwarded_auth,
    ));

    let manifest_arc = manifest.as_ref().map(|m| Arc::new(m.clone()));
    let rest_state = RestState {
        runtime: runtime.clone(),
        discovery: discovery.clone(),
        dispatcher: dispatcher.clone(),
        identity: identity.clone(),
        manifest: manifest_arc,
        metrics: metrics.clone(),
        oidc_signer: static_signer.clone(),
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
    // Vault was decommissioned with the move to Kamal, so there is a
    // single resolver: literals (dev) and `env://` refs injected by the
    // substrate (GCP Secret Manager → kamal `.kamal/secrets`). A
    // `vault://` ref in a manifest now fails boot closed.
    let resolver: Arc<dyn SecretResolver> = Arc::new(LiteralResolver);

    let mut chat_router: Option<axum::Router> = None;
    // #95: adapters that support agent-initiated proactive sends are
    // also inserted here, keyed by manifest adapter name, so the
    // `/v1/outbound` endpoint can resolve a courier without going
    // through the inbound webhook.
    let mut outbound_couriers: triton_adapters_http::outbound::CourierRegistry =
        std::collections::HashMap::new();
    // PR 34: Signal's I/O is a persistent socket, not an HTTP
    // webhook — its inbound runs as a tokio task spawned at boot,
    // not a router mount. Both shapes coexist in the same loop;
    // each adapter dispatches on `inbound.kind`.
    let mut socket_adapter_joins: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // PR 38: build the rasterizer client once for any adapter that
    // needs it (Telegram / Discord / WhatsApp all consume the same
    // out-of-process renderer per FR-A-11). The NFR-S-4 egress
    // allowlist check fires ONCE if any rasterizer-using adapter
    // is in the manifest — moving it to process boot would break
    // existing OIDC integration tests that don't enable chat
    // adapters (see realizations.md §7 "NFR-S-4 egress checks belong
    // next to the adapter that needs the egress").
    let rasterizer_client: Option<RasterizerClient> = {
        let needs_rasterizer = manifest
            .as_ref()
            .map(|m| {
                m.adapters.values().any(|a| {
                    // Only the WEBHOOK rasterizing adapters dial the
                    // rasterizer. The socket variants (Discord Gateway,
                    // WhatsApp Web bridge) reply text-only and never
                    // receive a rasterizer client, so a bridge-only
                    // deploy must not be forced to configure one
                    // (Codex review).
                    a.inbound.kind == InboundKind::Webhook
                        && matches!(
                            a.kind,
                            AdapterKind::Telegram
                                | AdapterKind::Discord
                                | AdapterKind::WhatsappCloud
                        )
                })
            })
            .unwrap_or(false);
        if needs_rasterizer {
            // NFR-S-4: the rasterizer is a network dependency in
            // the adapter hot path. Default `http://127.0.0.1:9320`
            // is fine for local dev; outside `local` env the
            // operator MUST point at a tailnet-resolved hostname
            // (any dotted host that isn't an IP literal — a
            // `*.tailnet.ts.net` will do). Refusing loopback /
            // public-by-default values stops a misconfigured deploy
            // from exfiltrating dashboard payloads to an unaudited
            // host. PR 37: parse the URL through the `url` crate so
            // a malformed env var fails closed at the parse step.
            if settings.env != "local" {
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
            // Build the client lazily: dashboards are an opt-in
            // feature, but the mapper declares
            // `dashboard: rasterised_png` in every manifest we
            // ship, so the URL is essentially always set. Boot
            // failure on a bad URL is fatal so dashboards can never
            // silently fall through to text (M-RASTER-1).
            match RasterizerClient::new(RasterizerConfig {
                base: settings.rasterizer_url.clone(),
                ..Default::default()
            }) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!(
                        rasterizer_url = %settings.rasterizer_url,
                        error = %e,
                        "rasterizer client build failed",
                    );
                    std::process::exit(2);
                }
            }
        } else {
            None
        }
    };
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
                    AdapterKind::Discord => {
                        // NFR-S-4 egress allowlist: outside `local`
                        // env the Gateway URL MUST be Discord's
                        // canonical WSS host and the REST base
                        // discord.com — no override that could point
                        // the socket (carrying the bot token) or the
                        // reply POST at an attacker host.
                        if settings.env != "local" {
                            if !is_canonical_url(
                                &settings.discord_gateway_url,
                                "wss",
                                "gateway.discord.gg",
                            ) {
                                tracing::error!(
                                    env = %settings.env,
                                    discord_gateway_url = %settings.discord_gateway_url,
                                    "non-`local` env MUST use TRITON_DISCORD_GATEWAY_URL=wss://gateway.discord.gg (NFR-S-4 egress allowlist)",
                                );
                                std::process::exit(2);
                            }
                            if !is_canonical_url(&settings.discord_api_base, "https", "discord.com")
                            {
                                tracing::error!(
                                    env = %settings.env,
                                    discord_api_base = %settings.discord_api_base,
                                    "non-`local` env MUST use TRITON_DISCORD_API_BASE with host discord.com (NFR-S-4 egress allowlist)",
                                );
                                std::process::exit(2);
                            }
                        }
                        match triton_chat_discord::DiscordGatewayAdapter::from_manifest(
                            name,
                            adapter,
                            resolver.as_ref(),
                            dispatcher.clone(),
                            settings.discord_gateway_url.clone(),
                            settings.discord_api_base.clone(),
                        )
                        .await
                        {
                            Ok(built) => {
                                tracing::info!(adapter = %name, "discord gateway socket adapter wired");
                                let h = Arc::new(built).spawn(shutdown.clone());
                                socket_adapter_joins.push(h);
                            }
                            Err(e) => {
                                tracing::error!(adapter = %name, error = %e, "discord gateway adapter build failed");
                                std::process::exit(2);
                            }
                        }
                    }
                    AdapterKind::WhatsappWeb => {
                        // The WhatsApp Web bridge daemon is a local
                        // sidecar terminating the WhatsApp session
                        // inside the trust boundary. NFR-S-4 / C-11
                        // (mirrors Signal): outside `local` the bridge
                        // addr MUST be a `unix://` path or a
                        // `tcp://*.ts.net` tailnet target — never an
                        // arbitrary host.
                        let addr = settings.whatsapp_bridge_addr.as_str();
                        if addr.is_empty() {
                            tracing::error!(
                                adapter = %name,
                                "whatsapp socket adapter requires TRITON_WHATSAPP_BRIDGE_ADDR (tcp://host:port or unix:///path)",
                            );
                            std::process::exit(2);
                        }
                        if settings.env != "local" {
                            let allowed = if is_unix_socket_addr(addr) {
                                true
                            } else {
                                let host = parse_host(addr).unwrap_or("");
                                !host.is_empty()
                                    && host
                                        .strip_suffix(".ts.net")
                                        .is_some_and(|prefix| !prefix.is_empty())
                            };
                            if !allowed {
                                tracing::error!(
                                    env = %settings.env,
                                    whatsapp_bridge_addr = %addr,
                                    "non-`local` env MUST set TRITON_WHATSAPP_BRIDGE_ADDR to a \
                                     `unix://...` path or a `tcp://*.ts.net[:port]` tailnet target \
                                     (NFR-S-4 / C-11 locality)",
                                );
                                std::process::exit(2);
                            }
                        }
                        match triton_chat_whatsapp::WhatsAppBridgeAdapter::from_manifest(
                            name,
                            adapter,
                            resolver.as_ref(),
                            dispatcher.clone(),
                            addr,
                        )
                        .await
                        {
                            Ok(built) => {
                                tracing::info!(adapter = %name, "whatsapp bridge socket adapter wired");
                                let h = Arc::new(built).spawn(shutdown.clone());
                                socket_adapter_joins.push(h);
                            }
                            Err(e) => {
                                tracing::error!(adapter = %name, error = %e, "whatsapp bridge adapter build failed");
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
            if adapter.inbound.kind == InboundKind::LongPoll {
                // FR-A-1.v0.2 `long_poll`: a worker that polls
                // getUpdates instead of receiving webhooks. Currently
                // only Telegram offers this as an alternative to its
                // webhook inbound.
                match adapter.kind {
                    AdapterKind::Telegram => {
                        // Same NFR-S-4 egress allowlist as the webhook
                        // path — the long-poll worker dials the SAME
                        // api base, so the guard must be identical.
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
                            rasterizer_client.clone(),
                        )
                        .await
                        {
                            Ok(built) => {
                                tracing::info!(adapter = %name, "telegram long-poll adapter wired");
                                let h = Arc::new(built).spawn_long_poll(shutdown.clone());
                                socket_adapter_joins.push(h);
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
                            "long_poll inbound not implemented for this adapter kind; skipping",
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
                    // PR 38: rasterizer client built once at the top
                    // of the chat-adapter wiring (shared with Discord
                    // + WhatsApp); the NFR-S-4 egress allowlist check
                    // also runs there if any rasterizer-using adapter
                    // is present. See realizations.md §7 "NFR-S-4
                    // egress checks belong next to the adapter that
                    // needs the egress".
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
                        rasterizer_client.clone(),
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
                        rasterizer_client.clone(),
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
                AdapterKind::WhatsappCloud => {
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
                        rasterizer_client.clone(),
                    )
                    .await
                    {
                        Ok(built) => {
                            tracing::info!(adapter = %name, "whatsapp webhook adapter wired");
                            // Keep an Arc for the outbound courier registry
                            // (#95) before `router()` consumes one.
                            let built = Arc::new(built);
                            outbound_couriers.insert(
                                name.to_string(),
                                built.clone() as Arc<dyn triton_core::OutboundCourier>,
                            );
                            let r = built.router();
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

    // In single-port mode the chat webhook rides the unified HTTP port
    // (merged into `combined` below) — the substrate model is one port per
    // host, so the webhook is reachable behind kamal-proxy's TLS at
    // `/<adapter>/webhook` without a second public listener. Only the
    // three-port path binds a dedicated chat listener.
    let chat_listener =
        if !settings.single_port && chat_router.is_some() && settings.chat_webhook_port != 0 {
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
    // #95: build the agent-initiated outbound surface. Its bearer must
    // carry the DEDICATED outbound audience (a second OIDC verifier),
    // so a token minted for the HTTP trio cannot drive proactive sends.
    // #100: the verifier may also trust a DEDICATED issuer
    // (TRITON_OUTBOUND_ISSUER, falling back to the trio's) — the
    // mirror image of static-upstream signing: the registered agent
    // signs its own short-TTL outbound tokens and serves JWKS on its
    // internal FQDN (TRITON_OUTBOUND_JWKS_URL skips OIDC discovery).
    // When an issuer is known but no outbound audience is configured
    // the endpoint stays unmounted (fail closed); in the no-OIDC dev
    // path it reuses the trio's dev-token identity for parity.
    let outbound_couriers = Arc::new(outbound_couriers);
    // #115: outbound rate limiters — a global floor (10× per-sec headroom,
    // mirroring the chat adapters) + a per-tenant fair-share gate. Shared
    // into whichever OutboundState we build below.
    let outbound_rate_limit = Arc::new(triton_core::ratelimit::TokenBucket::new(
        settings.outbound_rate_limit_per_sec.saturating_mul(10),
        settings.outbound_rate_limit_burst.saturating_mul(10),
    ));
    let outbound_per_tenant = Arc::new(triton_core::ratelimit::PerTenantBuckets::new(
        settings.outbound_rate_limit_per_sec,
        settings.outbound_rate_limit_burst,
    ));
    let outbound_issuer = settings
        .outbound_issuer
        .as_ref()
        .or(settings.oidc_issuer.as_ref());
    let outbound_state: Option<triton_adapters_http::outbound::OutboundState> =
        match (outbound_issuer, &settings.outbound_audience) {
            (Some(issuer), Some(aud)) => {
                tracing::info!(
                    issuer,
                    aud,
                    jwks_url = settings.outbound_jwks_url.as_deref(),
                    dedicated_issuer = settings.outbound_issuer.is_some(),
                    "outbound OIDC verifier enabled (/v1/outbound)"
                );
                let mut config = OidcConfig::new(issuer.clone(), aud.clone());
                if let Some(jwks_url) = &settings.outbound_jwks_url {
                    config = config.with_jwks_url(jwks_url.clone());
                }
                let verifier = Arc::new(OidcVerifier::new(config));
                Some(triton_adapters_http::outbound::OutboundState {
                    identity: Arc::new(IdentityProvider::new(Some(verifier))),
                    dispatcher: dispatcher.clone(),
                    couriers: outbound_couriers.clone(),
                    rate_limit: outbound_rate_limit.clone(),
                    per_tenant_limit: outbound_per_tenant.clone(),
                })
            }
            (Some(_), None) => {
                tracing::warn!(
                    "OIDC configured but TRITON_OUTBOUND_AUDIENCE unset; /v1/outbound disabled"
                );
                None
            }
            (None, _) => Some(triton_adapters_http::outbound::OutboundState {
                identity: identity.clone(),
                dispatcher: dispatcher.clone(),
                couriers: outbound_couriers.clone(),
                rate_limit: outbound_rate_limit.clone(),
                per_tenant_limit: outbound_per_tenant.clone(),
            }),
        };

    let mcp_router = triton_adapters_http::mcp::router(mcp_state);
    let a2a_router = triton_adapters_http::a2a::router(a2a_state);
    let rest_router = triton_adapters_http::rest::router(rest_state);
    let rest_router = match outbound_state {
        Some(os) => rest_router.merge(triton_adapters_http::outbound::router(os)),
        None => rest_router,
    };

    // Boxed serve-future type shared by all three HTTP listeners so the
    // `tokio::join!` shape is identical whether we bind three ports or one
    // (mirrors `metrics_fut`/`chat_fut` below).
    type ServeFut =
        std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;

    // A no-op serve future for an unbound listener: it just awaits the
    // shutdown signal and returns Ok, keeping the join shape valid.
    fn noop_serve(token: CancellationToken) -> ServeFut {
        Box::pin(async move {
            token.cancelled_owned().await;
            Ok(())
        })
    }

    let (serve_mcp, serve_a2a, serve_rest): (ServeFut, ServeFut, ServeFut) = if settings.single_port
    {
        use std::future::IntoFuture;
        // Single-port: compose REST at root + MCP at `/mcp` + A2A at `/a2a`
        // into one router (like triton-embed, but with the production
        // states). CORS, if any, wraps the COMBINED router; MCP and A2A
        // get no-op serve-futures since their ports are never bound.
        let mut combined = rest_router
            .nest("/mcp", mcp_router)
            .nest("/a2a", a2a_router);
        // Mount chat webhook adapters (e.g. `/whatsapp/webhook`) on the
        // same port so a single kamal-proxy TLS route fronts both the REST
        // trio and the inbound webhook (one public host, one cert).
        if let Some(cr) = chat_router.take() {
            combined = combined.merge(cr);
            tracing::info!("chat webhook adapters mounted on the unified HTTP port (single_port)");
        }
        if let Some(l) = &cors_layer {
            combined = combined.layer(l.clone());
        }
        let serve_rest: ServeFut = Box::pin(
            axum::serve(rest_listener, combined)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
        );
        (
            noop_serve(shutdown.clone()),
            noop_serve(shutdown.clone()),
            serve_rest,
        )
    } else {
        use std::future::IntoFuture;
        // Three-port default: each surface gets its own CORS-wrapped router
        // on its own listener (byte-for-byte the prior behavior).
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
        let mcp_listener = mcp_listener.expect("mcp listener bound in three-port mode");
        let a2a_listener = a2a_listener.expect("a2a listener bound in three-port mode");
        let serve_mcp: ServeFut = Box::pin(
            axum::serve(mcp_listener, mcp_router)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
        );
        let serve_a2a: ServeFut = Box::pin(
            axum::serve(a2a_listener, a2a_router)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
        );
        let serve_rest: ServeFut = Box::pin(
            axum::serve(rest_listener, rest_router)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
        );
        (serve_mcp, serve_a2a, serve_rest)
    };

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
    // Run the listeners + ancillary futures forever, then on signal
    // receipt bound the post-shutdown drain by `drain_deadline`. The
    // earlier shape wrapped the whole join in
    // `tokio::time::timeout(drain_deadline, ...)`, which capped total
    // uptime at `drain_deadline` regardless of signal — issue #65.
    let serve = async {
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
    tokio::pin!(serve);

    let serve_result = tokio::select! {
        // A listener returning early (e.g. axum::serve hit an
        // unexpected error) is an exit reason in itself — propagate
        // its result without starting the drain timer (nothing left
        // to drain).
        res = &mut serve => res,

        // Signal receipt → start the bounded drain. `signal_task`
        // cancels `shutdown`, which resolves every listener's
        // `with_graceful_shutdown(shutdown.cancelled_owned())` and
        // lets `serve` complete in-flight requests.
        _ = shutdown.cancelled() => {
            match tokio::time::timeout(settings.drain_deadline, &mut serve).await {
                Ok(res) => res,
                Err(_) => {
                    tracing::warn!(
                        deadline_secs = settings.drain_deadline.as_secs(),
                        "drain deadline exceeded; in-flight connections aborted"
                    );
                    Ok(())
                }
            }
        }
    };

    shutdown.cancel();
    let _ = signal_task.await;

    let _ = std::io::stdout().flush();

    tracing::info!("triton: exit");
    serve_result
}

/// Build the in-process tool registry, skipping any tool whose name is
/// claimed by a static upstream (`TRITON_STATIC_UPSTREAMS`). The
/// dispatcher prefers in-process tools over the upstream router, so a
/// shadowed registration would make the mapped agent unreachable.
fn build_registry(shadowed: &std::collections::HashSet<String>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    let mut register = |tool: Arc<dyn triton_core::Tool>| {
        if shadowed.contains(tool.name()) {
            tracing::info!(
                tool = tool.name(),
                "in-process tool shadowed by static upstream; skipping registration"
            );
            return;
        }
        registry.register(tool);
    };
    register(Arc::new(tools::Echo));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::Delay));
    register(Arc::new(tools::Narrate));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::DemoPanel));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::EmptySurface));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::FormOnlyDemo));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::FormOnlyDemoMulti));
    #[cfg(feature = "dev-token")]
    register(Arc::new(tools::SubmittedForm));
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
