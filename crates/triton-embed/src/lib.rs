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
//!
//! A host that needs the binary's full ingress — Entra multi-tenant with
//! required scopes, `/mcp/<slug>` with RFC 9728 metadata, spec A2A and the
//! task store from `TRITON_*` — sets [`EmbedOpts::ingress`] and calls
//! [`router_parts`], which also returns the task runner and identity
//! provider for further surfaces (feature `chat`: `chat::msteams`,
//! `surfaces::registry`). doc/upstream-agent-contract.md §8 describes the
//! in-process upstream such a host dispatches to.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[cfg(feature = "chat")]
pub mod chat;
pub mod ingress;
#[cfg(feature = "chat")]
pub mod surfaces;

use axum::Router;
use triton_adapters_http::a2a::{self, A2aState, InMemoryTaskStore};
use triton_adapters_http::a2a_spec::{self, SpecA2aConfig};
use triton_adapters_http::cors;
use triton_adapters_http::identity::IdentityProvider;
use triton_adapters_http::mcp::{self, McpSessions, McpState};
use triton_adapters_http::rest::{self, OidcProviderInfo, RestState, RuntimeDiscovery};
use triton_adapters_http::tasks::{Runner, TaskHub, TaskSettings};
use triton_core::{Dispatcher, RuntimeInfo, ToolRegistry};
use triton_identity::{GoogleAccessTokenVerifier, OidcConfig, OidcVerifier};

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
    /// Spec-A2A: the Agent Card's contents and the tool prose is
    /// dispatched to. `None` (the default) leaves BOTH the JSON-RPC
    /// route and the card unmounted, so the surface is exactly what it
    /// was before spec-A2A existed. Set it with [`EmbedOpts::spec_a2a`].
    pub spec_a2a: Option<SpecA2aConfig>,
    /// Per-agent spec-A2A faces (one Agent Card + JSON-RPC endpoint per
    /// agent, at `/a2a/<slug>`), so a platform that registers ONE card per
    /// agent (Gemini Enterprise) can address each roster agent directly.
    /// Each entry's `a2a_path` is `/a2a/<slug>`. Its `public_url` and
    /// `version` are NOT its own: they are taken from [`Self::spec_a2a`]
    /// when the router is built, so the cards cannot disagree about the
    /// origin. **Requires `spec_a2a`**: without the primary config these
    /// entries are ignored (a warning is logged) — there is no public URL
    /// to advertise. Add entries with [`EmbedOpts::spec_a2a_agent`].
    pub spec_a2a_agents: Vec<SpecA2aConfig>,
    /// Every accepted `(issuer, audience)` pair, for `/v1/runtime`,
    /// including when there is only one. The three scalars above keep
    /// reporting the FIRST pair unchanged — they are a published
    /// contract (ADR-0017's verification reads `oidc_issuer`), so
    /// multi-issuer adds a field rather than redefining one.
    pub oidc_providers: Vec<(String, String)>,
    /// Operators holding the cross-tenant view of `/v1/audit`, `/v1/trace`
    /// and A2A `tasks/get` — `(tenant, sub)` pairs.
    ///
    /// `None` means "read `TRITON_AUDIT_OPERATORS` from the process
    /// environment", which is what a standalone deployment wants. A host
    /// that embeds triton can pass them instead, which is the whole point:
    /// this value used to be read by `router()` from the environment and
    /// nowhere else, so an embedding host had no way to set it except by
    /// mutating its own process env — the same shape as the denylist that
    /// shipped dead (doc/realizations.md §9), and the reason the
    /// conjunction had no embedded test.
    pub audit_operators: Option<std::collections::HashSet<(String, String)>>,
    /// Optional fallback for opaque Google OAuth access tokens (Gemini
    /// Enterprise forwards these over A2A — they are not JWTs). Set with
    /// [`EmbedOpts::google_access`]. `None` = only JWTs are accepted.
    pub google_access: Option<Arc<GoogleAccessTokenVerifier>>,
    /// The self-signer for the JWTs this host mints to its OWN static
    /// upstreams (workload→workload, no Vault). When `Some`, the REST adapter
    /// serves `/.well-known/openid-configuration` + `/.well-known/jwks.json`
    /// from it, so an upstream can discover this host as an OIDC issuer and
    /// verify the tokens it mints. `None` (the default) leaves those routes
    /// 404, exactly as before. The standalone `triton-bin` already wires this
    /// through `RestState`; the embedded host could not, because `router()`
    /// hard-coded `oidc_signer: None` — an embedded host that reaches a static
    /// upstream (e.g. the DataZoo agent → anofox-evolve) therefore had no way
    /// to be an issuer at all. Set with [`EmbedOpts::oidc_signer`].
    pub oidc_signer: Option<Arc<triton_identity::JwtSigner>>,
    /// The binary's ingress: Entra multi-tenant with required scopes, the
    /// opaque Google access-token verifier, the per-customer MCP endpoints
    /// (`/mcp/<slug>` and their RFC 9728 metadata), spec A2A from
    /// `TRITON_A2A_*`, and the task store and timing. `None` (the default)
    /// keeps the embedded host as it was: memory tasks, default timing,
    /// only what the other fields configure. Set with
    /// [`EmbedOpts::ingress`]; read it with
    /// [`ingress::IngressConfig::from_env`].
    pub ingress: Option<ingress::IngressConfig>,
    /// Serve the answer-card MCP Apps widget (its resource and the tool
    /// `_meta` pointing at it). On by default, like the binary's
    /// `TRITON_MCP_APP_WIDGET`.
    pub widget: bool,
    /// The bearer the dev-token path accepts. `None` = the historical
    /// `dev-token` literal; empty disables the path. Only consulted in a
    /// `dev-token` build when no OIDC verifier is configured. Deliberately
    /// not printed by `Debug` (there is none on this struct).
    pub dev_token: Option<String>,
}

impl Default for EmbedOpts {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8088,
            env: "dev".to_string(),
            // Unset: read `TRITON_AUDIT_OPERATORS` from the environment.
            audit_operators: None,
            cors_origins: Vec::new(),
            oidc: Vec::new(),
            oidc_issuer: None,
            oidc_audience: None,
            oidc_client_id: None,
            oidc_providers: Vec::new(),
            spec_a2a: None,
            spec_a2a_agents: Vec::new(),
            google_access: None,
            oidc_signer: None,
            ingress: None,
            widget: true,
            dev_token: None,
        }
    }
}

impl EmbedOpts {
    /// Loopback, port 8088, dev env, same-origin CORS. Pair with the
    /// `dev-token` feature (default) for `Bearer dev-token` auth.
    pub fn dev() -> Self {
        Self::default()
    }

    /// Accept opaque Google OAuth access tokens (Gemini Enterprise / A2A) by
    /// introspection, in addition to the configured JWT OIDC pairs. `audience`
    /// is the Google OAuth client id the token's `aud` must match; `allowed_hd`
    /// is the required hosted domain (recommended). Opt-in; JWT paths unchanged.
    pub fn google_access(
        mut self,
        audience: impl Into<String>,
        allowed_hd: Option<String>,
    ) -> Self {
        self.google_access = Some(Arc::new(GoogleAccessTokenVerifier::new(
            audience, allowed_hd,
        )));
        self
    }

    /// Set the self-signer that makes this host an OIDC issuer for the JWTs it
    /// mints to its own static upstreams: the REST adapter then serves
    /// `/.well-known/openid-configuration` + `/.well-known/jwks.json` from it.
    pub fn oidc_signer(mut self, signer: Arc<triton_identity::JwtSigner>) -> Self {
        self.oidc_signer = Some(signer);
        self
    }

    /// Configure the binary's ingress (see the field). Errors in it — an
    /// override outside `env=local`, a customer path naming a tenant that
    /// is not allow-listed — surface from [`router_parts`].
    pub fn ingress(mut self, cfg: ingress::IngressConfig) -> Self {
        self.ingress = Some(cfg);
        self
    }

    /// Serve the answer-card widget or not (default on).
    pub fn widget(mut self, on: bool) -> Self {
        self.widget = on;
        self
    }

    /// The bearer the dev-token path accepts (see the field).
    pub fn dev_token(mut self, token: impl Into<String>) -> Self {
        self.dev_token = Some(token.into());
        self
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

    /// Add an **Entra multi-tenant** verifier (ADR-0021, #675): accepts any
    /// allow-listed Entra tenant for `audience` (issuer
    /// `https://login.microsoftonline.com/{tid}/v2.0`), mapping each `tid` to
    /// its data tenant. Never the human-login (pair-1) verifier — it is a
    /// machine-caller pair, so it does not touch the `oidc_issuer` scalars.
    /// The Agent Card / `/v1/runtime` advertise the sentinel issuer so the
    /// surface is visible without leaking the per-customer tenant list.
    pub fn oidc_entra_multitenant(
        mut self,
        audience: impl Into<String>,
        tenant_map: std::collections::HashMap<String, String>,
    ) -> Self {
        let audience = audience.into();
        let cfg = OidcConfig::entra_multi_tenant(audience.clone(), tenant_map);
        self.oidc_providers.push((
            triton_identity::ENTRA_MULTI_TENANT_ISSUER.to_string(),
            audience,
        ));
        self.oidc.push(Arc::new(OidcVerifier::new(cfg)));
        self
    }

    /// Turn on the spec-conformant A2A face: JSON-RPC `message/send` +
    /// `tasks/get` at the A2A base path, and the Agent Card at
    /// `/.well-known/agent-card.json` (and `/.well-known/agent.json`).
    ///
    /// `public_url` is the externally reachable origin — the card must
    /// advertise where CALLERS reach this agent, which a process behind
    /// an ingress cannot infer from a request. `default_tool` is the one
    /// tool plain text is dispatched to; the caller never names a tool.
    pub fn spec_a2a(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        public_url: impl Into<String>,
        default_tool: impl Into<String>,
    ) -> Self {
        self.spec_a2a = Some(SpecA2aConfig {
            name: name.into(),
            description: description.into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            public_url: public_url.into(),
            a2a_path: "/a2a".to_string(),
            default_tool: default_tool.into(),
            skills: None,
        });
        self
    }

    /// Add a per-agent spec-A2A face: JSON-RPC at `POST /a2a/<slug>`
    /// dispatching prose to `tool`, and an Agent Card at
    /// `/a2a/<slug>/.well-known/<name>` (every spelling the primary card
    /// serves) whose `url` is `<public_url>/a2a/<slug>` and whose
    /// name/description are this agent's. Auth is the same verifier stack
    /// as the root `/a2a`.
    ///
    /// `slug` must be `[a-z0-9-]+` and not reserved
    /// ([`a2a_spec::validate_agent_slug`]); a bad or duplicate slug is an
    /// error here, at boot, rather than a route that silently never
    /// matches. The public URL and version come from [`Self::spec_a2a`]
    /// when the router is built; without it the agents are ignored (see
    /// the field's note). The root `/a2a` and root card are unchanged.
    pub fn spec_a2a_agent(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        slug: impl Into<String>,
        tool: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let slug = slug.into();
        a2a_spec::validate_agent_slug(&slug)?;
        let a2a_path = format!("/a2a/{slug}");
        if self.spec_a2a_agents.iter().any(|a| a.a2a_path == a2a_path) {
            anyhow::bail!("a2a agent slug `{slug}` is configured twice");
        }
        self.spec_a2a_agents.push(SpecA2aConfig {
            name: name.into(),
            description: description.into(),
            // Filled from the primary `spec_a2a` config in `router()`.
            version: String::new(),
            public_url: String::new(),
            a2a_path,
            default_tool: tool.into(),
            skills: None,
        });
        Ok(self)
    }

    /// Override the client ID advertised at `/v1/runtime` when it is not
    /// the same string as the audience.
    /// Name the cross-tenant audit operators explicitly instead of
    /// reading `TRITON_AUDIT_OPERATORS`. See the field's note.
    pub fn audit_operators(
        mut self,
        operators: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.audit_operators = Some(operators.into_iter().collect());
        self
    }

    pub fn oidc_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.oidc_client_id = Some(client_id.into());
        self
    }
}

/// What [`router_parts`] builds: the single-port router, plus the pieces a
/// host needs to wire further surfaces into the same process — the task
/// runner (one store and hub shared by A2A, MCP and, through
/// `chat::msteams`, Teams) and the inbound identity boundary.
pub struct EmbedParts {
    pub router: Router,
    /// Drives long-running tasks; the store is SQLite when the ingress
    /// names `TRITON_TASK_DB`, else process memory.
    pub runner: Runner,
    /// The identity provider every route of `router` enforces.
    pub identity: Arc<IdentityProvider>,
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
///
/// Panics when [`EmbedOpts::ingress`] is refused; a host that sets it
/// should call [`router_parts`], which returns the error instead.
pub fn router(dispatcher: Arc<Dispatcher>, opts: &EmbedOpts) -> Router {
    match router_parts(dispatcher, opts) {
        Ok(parts) => parts.router,
        Err(e) => panic!("triton-embed: {e}; refusing to serve"),
    }
}

/// Like [`router`], but returns the task runner and identity provider
/// beside the router, and a refused [`EmbedOpts::ingress`] as an error
/// naming the variable.
pub fn router_parts(dispatcher: Arc<Dispatcher>, opts: &EmbedOpts) -> anyhow::Result<EmbedParts> {
    // The ingress is validated before anything process-wide happens, so a
    // refused configuration leaves no trace. Same order as the binary:
    // customer paths, identity providers, scopes, tasks, spec A2A.
    let mut verifiers = opts.oidc.clone();
    let mut providers = opts.oidc_providers.clone();
    let mut google_access = opts.google_access.clone();
    let mut scopes: Vec<String> = Vec::new();
    let mut mcp_routes = Vec::new();
    let mut spec_a2a = opts.spec_a2a.clone();
    let (task_store, task_settings) = match &opts.ingress {
        None => (InMemoryTaskStore::new(), TaskSettings::default()),
        Some(cfg) => {
            mcp_routes = ingress::mcp_routes(cfg, opts.oidc_issuer.as_deref())?;
            if let Some(g) =
                ingress::identity_providers(cfg, &opts.env, &mut verifiers, &mut providers)?
            {
                google_access = Some(g);
            }
            scopes = ingress::entra_scopes(cfg)?;
            let store = ingress::task_store(cfg)?;
            // An explicit `EmbedOpts::spec_a2a` wins over the variables.
            if spec_a2a.is_none() {
                spec_a2a = ingress::spec_a2a_config(cfg)?;
            }
            (store, ingress::task_settings(cfg))
        }
    };

    // F4: the A2UI action key, injected once per process from the host's
    // environment. A malformed value refuses to serve (a random fallback
    // would break every click that lands on another replica, silently).
    static ACTION_KEY: std::sync::Once = std::sync::Once::new();
    ACTION_KEY.call_once(|| {
        match triton_core::a2ui::ge::configure_action_key(
            std::env::var("TRITON_A2UI_ACTION_KEY").ok().as_deref(),
        ) {
            Ok(triton_core::a2ui::ge::ActionKeyState::Configured) => {}
            Ok(triton_core::a2ui::ge::ActionKeyState::Ephemeral) => eprintln!(
                "triton-embed: TRITON_A2UI_ACTION_KEY unset: A2UI card actions resolve on \
                 this single replica only"
            ),
            Err(e) => panic!("TRITON_A2UI_ACTION_KEY is invalid ({e}); refusing to serve"),
        }
    });
    // #306 crew F7: `router` is a public entry point — an embedding host
    // that merges its own routes (dz-agent-template does) calls this and
    // never `serve_dispatcher`, so announcing only there left exactly the
    // hosts this stack was built for silent. Announced once per process:
    // a host that builds two routers should not warn twice.
    // Resolved ONCE, and shared by every consumer below. It used to be
    // three independent `from_env()` calls in this function — announce,
    // REST, A2A — which is how a value can be announced and then not be
    // the one enforced. One parse, one Arc.
    let audit_operators: Arc<std::collections::HashSet<(String, String)>> = Arc::new(
        opts.audit_operators
            .clone()
            .unwrap_or_else(|| triton_config::DeploymentConfig::from_env().audit_operators),
    );
    let announced_operators = audit_operators.clone();
    let announced_env = opts.env.clone();
    let announced_denied: Vec<String> = dispatcher.denied_principals().collect();
    let announced_enforcing = dispatcher.is_enforcing();
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    ANNOUNCED.call_once(move || {
        triton_config::announce(
            &announced_env,
            &announced_operators,
            announced_denied.into_iter(),
            announced_enforcing,
        );
    });
    let mut identity = IdentityProvider::with_verifiers(verifiers, false);
    if let Some(token) = &opts.dev_token {
        identity = identity.with_dev_token(token.clone());
    }
    // ADR-22: a 403 from the Entra scope check names the scope.
    if !scopes.is_empty() {
        identity = identity
            .with_scope_challenge(&scopes)
            .map_err(|e| anyhow::anyhow!("TRITON_ENTRA_SCOPES: {e}"))?;
    }
    if let Some(g) = google_access {
        identity = identity.with_google_access(g);
    }
    let identity = Arc::new(identity);
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
        oidc_providers: providers
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
        // Parsed once above, not per request (#306 crew F7).
        audit_operators: audit_operators.clone(),
        // When the embedding host sets a self-signer (it reaches a static
        // upstream and must be an OIDC issuer that upstream can verify), serve
        // discovery + JWKS from it; otherwise those routes stay 404 as before.
        oidc_signer: opts.oidc_signer.clone(),
    };
    let dispatcher_for_card = dispatcher.clone();
    let a2a_state = A2aState {
        dispatcher: dispatcher.clone(),
        tasks: task_store,
        hub: TaskHub::new(),
        task_settings: Arc::new(task_settings),
        identity: identity.clone(),
        // #306 crew F1: `tasks/get` scopes on a trace id.
        audit_operators,
    };
    let runner = a2a_state.runner();
    let mcp_state = McpState {
        dispatcher,
        sessions: McpSessions::new(),
        identity: identity.clone(),
        // MCP files long-running upstream calls as tasks in the same store.
        tasks: Some(runner.clone()),
        route: None,
        widget: opts.widget,
    };
    // One MCP endpoint per customer (`/mcp/<slug>` + RFC 9728 metadata),
    // at absolute paths; empty without `TRITON_MCP_TENANTS`.
    let mcp_tenant_router =
        triton_adapters_http::mcp_tenants::router(mcp_state.clone(), &mcp_routes);

    // The spec-A2A JSON-RPC route answers at the A2A BASE path itself
    // (`POST /a2a`), which is what an Agent Card's `url` points at,
    // while the Triton-shaped `/a2a/message:send` keeps its own path.
    // The card is ALSO served endpoint-relative here (→ merged under the
    // `/a2a` nest below as `/a2a/.well-known/<name>`) because Copilot
    // Studio's runtime resolves the card relative to the registered
    // endpoint URL, not the origin (see a2a_spec::card_router).
    let a2a_router = match &spec_a2a {
        Some(cfg) => {
            let mut r = a2a::router(a2a_state.clone()).merge(a2a_spec::spec_routes(
                a2a_state.clone(),
                cfg.clone(),
                a2a_spec::card_state(cfg.clone(), dispatcher_for_card.clone(), providers.clone()),
            ));
            // The per-agent faces hang off the origin: with an
            // `EmbedOpts::spec_a2a` config the public URL IS the origin
            // (its path is `/a2a`); one from the ingress names the
            // endpoint itself (an empty path), so take its origin.
            let agents_origin = if cfg.a2a_path.is_empty() {
                url::Url::parse(&cfg.public_url)
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_else(|_| cfg.public_url.clone())
            } else {
                cfg.public_url.clone()
            };
            // Per-agent faces at `/a2a/<slug>`: same state (identity, task
            // store) as the root route, origin + version from the primary.
            for agent in &opts.spec_a2a_agents {
                let slug = agent.a2a_path.trim_start_matches("/a2a/");
                let agent_cfg = SpecA2aConfig {
                    public_url: agents_origin.clone(),
                    version: cfg.version.clone(),
                    ..agent.clone()
                };
                let card =
                    a2a_spec::card_state(agent_cfg, dispatcher_for_card.clone(), providers.clone());
                match a2a_spec::agent_router(slug, a2a_state.clone(), card) {
                    Ok(agent_router) => r = r.merge(agent_router),
                    // Unreachable through `spec_a2a_agent` (it validates);
                    // only a hand-built `spec_a2a_agents` entry gets here.
                    Err(e) => eprintln!(
                        r#"{{"kind":"log","level":"warn","msg":"skipping a2a agent","error":"{e}"}}"#
                    ),
                }
            }
            r
        }
        None => {
            if !opts.spec_a2a_agents.is_empty() {
                eprintln!(
                    r#"{{"kind":"log","level":"warn","msg":"spec_a2a_agents ignored: spec_a2a (public URL) is not configured","count":{}}}"#,
                    opts.spec_a2a_agents.len()
                );
            }
            a2a::router(a2a_state)
        }
    };

    let mut app = rest::router(rest_state)
        .nest("/mcp", mcp::router(mcp_state))
        .merge(mcp_tenant_router)
        .nest("/a2a", a2a_router);

    // The card is also mounted at the HOST ROOT: browser-time discovery
    // looks at `<origin>/.well-known/...`.
    if let Some(cfg) = &spec_a2a {
        app = app.merge(a2a_spec::card_router(a2a_spec::card_state(
            cfg.clone(),
            dispatcher_for_card,
            providers.clone(),
        )));
    }

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
    Ok(EmbedParts {
        router: app,
        runner,
        identity,
    })
}

/// Build a dispatcher from `reg` and serve the trio (+ `/explorer`) on one
/// port until the process exits.
pub async fn serve(reg: ToolRegistry, opts: EmbedOpts) -> anyhow::Result<()> {
    let config = triton_config::DeploymentConfig::from_env();
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(reg),
        opts.env.clone(),
        config.controls,
    ));
    serve_dispatcher(dispatcher, opts).await
}

/// Like [`serve`], but for a pre-built [`Dispatcher`] (e.g. one wired with
/// `.with_upstream(...)`).
pub async fn serve_dispatcher(dispatcher: Arc<Dispatcher>, opts: EmbedOpts) -> anyhow::Result<()> {
    // #287: report the controls in force before accepting traffic. An
    // embedded host that builds its own router instead of calling this
    // must call `announce_controls` itself — the runbook says to check
    // for this line, and its absence must mean "not engaged", never
    // "engaged but nobody said so".
    // The announcement lives in `router` (#306 crew F7), which this
    // calls — a host that merges its own routes reaches `router` and
    // never gets here, and announcing in both would say it twice.
    let addr = SocketAddr::new(opts.host, opts.port);
    let app = router_parts(dispatcher, &opts)?.router;
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
