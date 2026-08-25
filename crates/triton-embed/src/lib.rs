//! Embed Triton's HTTP trio (and optionally the Explorer) in-process.
//!
//! Issue #75, Mode 1: a Rust agent registers its in-process [`Tool`]s and
//! gets the full REST/MCP/A2A surface — plus the Explorer dev console —
//! from a single binary on a single port, with no Consul/Vault.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use triton_core::ToolRegistry;
//! # async fn run(reg: ToolRegistry) -> anyhow::Result<()> {
//! triton_embed::serve(reg, triton_embed::EmbedOpts::dev().port(8088)).await
//! # }
//! ```
//!
//! Layout on the one port: REST at the root (`/v1/tools/…`, `/healthz`,
//! `/v1/runtime`, …), MCP nested at `/mcp`, A2A nested at `/a2a`
//! (`/a2a/message:send`), and — with the `explorer-assets` feature — the
//! SPA at `/explorer`. This mirrors `triton-bin`'s wiring but composes
//! everything into one [`axum::Router`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use triton_adapters_http::a2a::{self, A2aState, InMemoryTaskStore};
use triton_adapters_http::cors;
use triton_adapters_http::identity::IdentityProvider;
use triton_adapters_http::mcp::{self, McpSessions, McpState};
use triton_adapters_http::rest::{self, OidcProviderInfo, RestState, RuntimeDiscovery};
use triton_core::{Dispatcher, RuntimeInfo, ToolRegistry};
use triton_identity::{OidcConfig, OidcVerifier};

/// Options for the embedded host.
pub struct EmbedOpts {
    pub host: IpAddr,
    pub port: u16,
    pub env: String,
    /// Extra browser origins to allow via CORS. Empty = same-origin only
    /// (the embedded `/explorer` needs none). Set this if you run the SPA
    /// from a different origin (e.g. `flutter run`).
    pub cors_origins: Vec<String>,
    /// The inbound identity boundary: the accepted issuer/audience
    /// pairs, in configuration order. EMPTY = dev-token-only, which a
    /// release build (`--no-default-features`) compiles out — so a
    /// production host that leaves this empty serves NOTHING. Build it
    /// with [`EmbedOpts::oidc`], called once per accepted pair.
    pub oidc: Vec<Arc<OidcVerifier>>,
    /// Reported at `/v1/runtime` so a caller (and the Explorer SPA) can
    /// discover how to authenticate. `oidc_issuer: null` there is the
    /// symptom of an embedded host with no identity configured.
    pub oidc_issuer: Option<String>,
    pub oidc_audience: Option<String>,
    pub oidc_client_id: Option<String>,
    /// Every accepted `(issuer, audience)` pair, for `/v1/runtime`,
    /// including when there is only one. The three scalars above keep
    /// reporting the FIRST pair unchanged — they are a published
    /// contract (ADR-0017's verification reads `oidc_issuer`), so
    /// multi-issuer adds a field rather than redefining one.
    pub oidc_providers: Vec<(String, String)>,
}

impl Default for EmbedOpts {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8088,
            env: "dev".to_string(),
            cors_origins: Vec::new(),
            oidc: Vec::new(),
            oidc_issuer: None,
            oidc_audience: None,
            oidc_client_id: None,
            oidc_providers: Vec::new(),
        }
    }
}

impl EmbedOpts {
    /// Loopback, port 8088, dev env, same-origin CORS. Pair with the
    /// `dev-token` feature (default) for `Bearer dev-token` auth.
    pub fn dev() -> Self {
        Self::default()
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn host(mut self, host: IpAddr) -> Self {
        self.host = host;
        self
    }

    pub fn env(mut self, env: impl Into<String>) -> Self {
        self.env = env.into();
        self
    }

    pub fn cors_origins(mut self, origins: Vec<String>) -> Self {
        self.cors_origins = origins;
        self
    }

    /// Verify inbound bearers as OIDC tokens from `issuer`, addressed to
    /// `audience`. This is what makes the embedded host usable in
    /// production: once set, [`IdentityProvider`] treats OIDC as the ONLY
    /// accepted identity and the dev-token path is closed even in a
    /// `dev-token` build.
    ///
    /// `jwks_url` is optional — leave it `None` for any issuer that
    /// publishes `/.well-known/openid-configuration` (Google does), and
    /// set it only for issuers that serve a bare JWKS. Note that Triton's
    /// own JWKS path is `/.well-known/jwks.json`; `/jwks.json` belongs to
    /// a different service and yields a silent 404 where every token then
    /// fails to verify.
    pub fn oidc(
        mut self,
        issuer: impl Into<String>,
        audience: impl Into<String>,
        jwks_url: Option<String>,
    ) -> Self {
        let issuer = issuer.into();
        let audience = audience.into();
        let mut cfg = OidcConfig::new(issuer.clone(), audience.clone());
        if let Some(url) = jwks_url {
            cfg = cfg.with_jwks_url(url);
        }
        self.oidc.push(Arc::new(OidcVerifier::new(cfg)));
        self.oidc_providers.push((issuer.clone(), audience.clone()));
        // The scalars describe the FIRST pair only, and later calls must
        // not overwrite them: `/v1/runtime`'s `oidc_issuer` is what
        // ADR-0017's verification reads, and the Explorer SPA redirects
        // PKCE at it. Pair 1 is the human-login pair by convention;
        // additional pairs are machine callers that never see this.
        if self.oidc_issuer.is_none() {
            self.oidc_issuer = Some(issuer);
            // For Google (and most public IdPs) the OAuth client ID *is*
            // the audience, so default it here; `oidc_client_id`
            // overrides for issuers where the two differ.
            self.oidc_client_id.get_or_insert_with(|| audience.clone());
            self.oidc_audience = Some(audience);
        }
        self
    }

    /// Override the client ID advertised at `/v1/runtime` when it is not
    /// the same string as the audience.
    pub fn oidc_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.oidc_client_id = Some(client_id.into());
        self
    }
}

/// Compose the single-port router for an already-built [`Dispatcher`].
/// Public so tests (and advanced hosts) can drive it without binding a
/// port.
///
/// Identity comes from [`EmbedOpts::oidc`]. When it is unset the host is
/// dev-token-only — fine for `cargo run`, but a release build compiles the
/// dev token out, so an unset issuer in production means every request is
/// rejected while `/healthz` keeps answering. `/v1/runtime` reports the
/// issuer precisely so that state is visible rather than inferred.
pub fn router(dispatcher: Arc<Dispatcher>, opts: &EmbedOpts) -> Router {
    let identity = Arc::new(IdentityProvider::with_verifiers(opts.oidc.clone(), false));
    let metrics = dispatcher.metrics();
    let version = env!("CARGO_PKG_VERSION").to_string();

    let runtime = Arc::new(RuntimeInfo {
        binary_sha: "embedded".to_string(),
        image_sha: None,
        env: opts.env.clone(),
        package_version: version.clone(),
    });
    let discovery = Arc::new(RuntimeDiscovery {
        env: opts.env.clone(),
        image_sha: None,
        package_version: version,
        binary_sha: "embedded".to_string(),
        oidc_issuer: opts.oidc_issuer.clone(),
        oidc_audience: opts.oidc_audience.clone(),
        oidc_client_id: opts.oidc_client_id.clone(),
        oidc_providers: opts
            .oidc_providers
            .iter()
            .map(|(i, a)| OidcProviderInfo {
                issuer: i.clone(),
                audience: a.clone(),
            })
            .collect(),
        // Single-port host: MCP/A2A are nested under these paths, so the
        // SPA reaches the whole trio same-origin (no port-swap).
        mcp_base: Some("/mcp".to_string()),
        a2a_base: Some("/a2a".to_string()),
    });

    let rest_state = RestState {
        runtime,
        discovery,
        dispatcher: dispatcher.clone(),
        identity: identity.clone(),
        manifest: None,
        metrics,
        // The embedded single-port host doesn't do static-upstream signing.
        oidc_signer: None,
    };
    let mcp_state = McpState {
        dispatcher: dispatcher.clone(),
        sessions: McpSessions::new(),
        identity: identity.clone(),
    };
    let a2a_state = A2aState {
        dispatcher,
        tasks: InMemoryTaskStore::new(),
        identity,
    };

    let mut app = rest::router(rest_state)
        .nest("/mcp", mcp::router(mcp_state))
        .nest("/a2a", a2a::router(a2a_state));

    #[cfg(feature = "explorer-assets")]
    {
        app = app.merge(explorer::router());
    }

    // Dev body-capture for the Trace view (feature-gated, never release).
    #[cfg(feature = "capture")]
    {
        app = triton_adapters_http::capture::apply(app);
    }

    if let Some(layer) = cors::build_layer(&opts.cors_origins) {
        app = app.layer(layer);
    }
    app
}

/// Build a dispatcher from `reg` and serve the trio (+ `/explorer`) on one
/// port until the process exits.
pub async fn serve(reg: ToolRegistry, opts: EmbedOpts) -> anyhow::Result<()> {
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), opts.env.clone()));
    serve_dispatcher(dispatcher, opts).await
}

/// Like [`serve`], but for a pre-built [`Dispatcher`] (e.g. one wired with
/// `.with_upstream(...)`).
pub async fn serve_dispatcher(dispatcher: Arc<Dispatcher>, opts: EmbedOpts) -> anyhow::Result<()> {
    let addr = SocketAddr::new(opts.host, opts.port);
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    eprintln!(
        r#"{{"kind":"log","level":"info","msg":"triton-embed listening","addr":"{actual}","explorer":"http://{actual}/explorer"}}"#
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(feature = "explorer-assets")]
mod explorer {
    //! Serve the compiled Flutter SPA (built with `--base-href /explorer/`)
    //! at `/explorer`. SPA routes fall back to `index.html`.
    use axum::Router;
    use axum::extract::Path;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use rust_embed::RustEmbed;

    // Path is resolved relative to this crate's CARGO_MANIFEST_DIR by
    // rust-embed; `apps/explorer/build/web` must be built first
    // (`flutter build web --base-href /explorer/`).
    #[derive(RustEmbed)]
    #[folder = "../../apps/explorer/build/web"]
    struct Assets;

    pub fn router() -> Router {
        Router::new()
            .route("/explorer", get(|| serve("index.html".to_string())))
            .route("/explorer/", get(|| serve("index.html".to_string())))
            .route("/explorer/{*path}", get(|Path(p): Path<String>| serve(p)))
    }

    async fn serve(path: String) -> Response {
        let file = Assets::get(&path).or_else(|| Assets::get("index.html"));
        match file {
            Some(content) => {
                let mime = mime_guess::from_path(&path).first_or_octet_stream();
                ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}
