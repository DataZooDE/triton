//! Process-wide settings. 12-factor §III says config comes from env
//! vars; precedence per NFR-O-1 is `CLI flag > TRITON_* env var >
//! compile-time default`. `clap` evaluates flags first, then falls
//! back to env vars marked with `#[arg(env = "TRITON_...")]`, then
//! to the documented default.

use std::net::IpAddr;
use std::time::Duration;

use clap::Parser;

/// Process-wide settings, parsed once at startup and shared as
/// `Arc<Settings>`. Deliberately *not* `Clone` — the Rust port
/// realization (§2) calls out that making config cloneable is
/// wasteful when it's effectively immutable; `Arc<Settings>` is the
/// shared-ownership story.
#[derive(Debug)]
pub struct Settings {
    pub host: IpAddr,
    pub mcp_port: u16,
    pub a2a_port: u16,
    pub rest_port: u16,
    pub drain_deadline: Duration,
    /// Environment label baked into audit lines. Defaults to `local`
    /// when neither `--env` nor `TRITON_ENV` is supplied — substrate
    /// jobs set this to `nonprod` or `prod` via the Nomad template.
    pub env: String,
    /// Golden-image SHA, baked into the Nomad job env at
    /// `nomad job run` time by the substrate Packer step
    /// (architecture.md §7). `None` if not set (e.g. local dev).
    pub image_sha: Option<String>,
    pub oidc_issuer: Option<String>,
    pub oidc_audience: Option<String>,
    /// Dedicated `aud` claim for the agent-initiated outbound surface
    /// (`POST /v1/outbound`, #95). Distinct from `oidc_audience` so a
    /// token minted for the HTTP trio cannot drive proactive sends.
    /// When OIDC is on but this is unset, `/v1/outbound` is disabled.
    pub outbound_audience: Option<String>,
    /// Dedicated issuer for the outbound surface (#100). When set, the
    /// `/v1/outbound` verifier trusts THIS issuer instead of
    /// `oidc_issuer` — per-surface issuer, exactly like the existing
    /// per-surface audience. Unset → falls back to `oidc_issuer`
    /// (today's behaviour). Enables mirror-image static-upstream
    /// signing: the upstream agent signs its own short-TTL outbound
    /// tokens and serves JWKS on its internal FQDN.
    pub outbound_issuer: Option<String>,
    /// Explicit JWKS document URL for the outbound verifier (#100).
    /// When set, key refresh fetches this URL directly instead of
    /// walking OIDC discovery — for agent issuers that serve only a
    /// JWKS document (e.g. `/.well-known/jwks.json`).
    pub outbound_jwks_url: Option<String>,
    /// #115: per-tenant rate limit for `POST /v1/outbound`
    /// (messages/sec + burst). The global floor is 10× the per-sec rate.
    /// Defaults 25/50, mirroring the chat adapters.
    pub outbound_rate_limit_per_sec: u32,
    pub outbound_rate_limit_burst: u32,
    pub upstream_timeout: Duration,
    pub circuit_open_after: u32,
    pub circuit_cooldown: Duration,
    pub metrics_host: IpAddr,
    pub metrics_port: u16,
    pub manifest_path: Option<std::path::PathBuf>,
    pub chat_webhook_host: IpAddr,
    pub chat_webhook_port: u16,
    pub telegram_api_base: String,
    pub whatsapp_api_base: String,
    pub discord_gateway_url: String,
    pub discord_api_base: String,
    pub whatsapp_bridge_addr: String,
    /// JWKS URI for Google Chat inbound JWT verification. Production
    /// stays at the canonical Google host (`https://www.googleapis.com/...`),
    /// which is the only host on the NFR-S-4 egress allowlist for
    /// this adapter. Integration tests override this with an in-repo
    /// `FakeGoogleJwks` fixture.
    pub google_chat_jwks_uri: String,
    /// signald daemon address override for the Signal adapter
    /// (PR 34). Empty string ⇒ use the address declared in
    /// `adapter.yaml`. Outside `local` env this MUST be set
    /// per NFR-S-4 egress allowlist — the binary refuses to wire
    /// the Signal adapter when the env is non-`local` and this
    /// override is empty.
    pub signal_signald_addr: String,
    /// OpenID discovery URL for Microsoft Bot Framework JWT
    /// verification. Production stays at the canonical Microsoft
    /// endpoint (the only `login.botframework.com` URL on the
    /// NFR-S-4 egress allowlist for this adapter). Integration
    /// tests override this with their `FakeBotFramework` fixture.
    pub msteams_openid_url: String,
    /// Optional override for the Microsoft OAuth2 token endpoint.
    /// `None` ⇒ use the hardcoded canonical endpoint inside
    /// `token_client.rs`. Tests set this so the fake bot framework
    /// can mint stub access tokens. PR 37: NFR-S-4 — the binary
    /// refuses any value outside `local` env so a compromised env
    /// var can't post `client_credentials` at an attacker host.
    pub msteams_token_url: Option<String>,
    /// PR 37: comma-separated extra hosts the MS Teams JWT verifier
    /// will accept on the inbound `serviceUrl` claim beyond Microsoft's
    /// documented suffixes (`.botframework.com`, `.trafficmanager.net`).
    /// Empty by default. The binary refuses non-empty values outside
    /// `local` env (NFR-S-4 egress allowlist for the outbound reply
    /// Activity).
    pub msteams_extra_service_url_hosts: Vec<String>,
    pub courier_timeout: Duration,
    /// Comma-separated allow-list of origins that may make
    /// cross-origin requests to the HTTP trio. Empty by default —
    /// in that case no CORS layer is mounted at all (production
    /// parity for substrate deployments that don't need a browser
    /// frontend). Set in nonprod to enable the Flutter explorer.
    pub cors_allowed_origins: Vec<String>,
    /// PR 36: base URL of the out-of-process dashboard rasterizer
    /// (FR-A-11). Default `http://127.0.0.1:9320` keeps local-dev
    /// drop-in: `cargo run --bin triton-rasterizer` in one shell,
    /// `cargo run --bin triton` in another. Outside `local` env
    /// the operator MUST set this to a `*.<tailnet>` hostname or
    /// boot fails (NFR-S-4 egress allowlist).
    pub rasterizer_url: String,
    /// OIDC `client_id` the Flutter explorer should use for its PKCE
    /// login. Reported at `GET /v1/runtime` so the SPA doesn't bake
    /// it in. `None` ⇒ the operator hasn't enabled the explorer in
    /// this env; the discovery endpoint returns null and the SPA
    /// shows an "ask an operator" message.
    pub explorer_client_id: Option<String>,
    /// Trust `X-Forwarded-Email` from a co-located oauth2-proxy
    /// sidecar (ADR-0011 / auth-portal-dz idiom). Issue #67: with
    /// this off, the SPA has to send a `Bearer` token even though
    /// the operator already authenticated at the sidecar. Default
    /// `false` for production parity — only safe to enable when the
    /// HTTP listeners are bound to loopback inside a Nomad alloc
    /// (so the only thing that can set the header is the sidecar in
    /// the shared netns).
    pub trust_forwarded_auth: bool,
    /// Single-port serving mode. When `true`, `triton-bin` serves the full
    /// HTTP trio on the REST port path-based — REST at root, MCP nested at
    /// `/mcp`, A2A nested at `/a2a` — and advertises `mcp_base`/`a2a_base`
    /// in `/v1/runtime` so the SPA reaches the whole trio same-origin. The
    /// MCP and A2A listeners are not bound in this mode. Default `false`
    /// keeps the three-separate-ports behavior byte-for-byte unchanged.
    pub single_port: bool,
    /// RSA private key PEM that signs the JWTs Triton mints to agents in
    /// static-upstream mode (no Vault). When set together with
    /// `static_upstream_issuer` and `jwt_jwks`, Triton signs per-call OIDC
    /// tokens instead of sending the static bearer, and serves JWKS so agents
    /// verify them. A shared key across instances keeps the served JWKS
    /// consistent behind a load balancer.
    pub jwt_signing_key: Option<String>,
    /// Public JWKS JSON (matching `jwt_signing_key`, same `kid`) served at
    /// `/.well-known/jwks.json` so agents can verify Triton's minted tokens.
    pub jwt_jwks: Option<String>,
    /// `kid` set in minted token headers; must match the key in `jwt_jwks`.
    pub jwt_kid: Option<String>,
    /// Issuer URL Triton advertises (token `iss` + discovery `issuer`). Agents
    /// set `AGENT_OIDC_ISSUER` to this and reach it for discovery/JWKS.
    pub static_upstream_issuer: Option<String>,
    /// `aud` claim for minted static-upstream tokens — the agent's expected
    /// audience. Defaults to `agents-<env>` when unset.
    pub static_upstream_aud: Option<String>,
    /// `tenant` claim for minted static-upstream tokens. A downstream the agent
    /// forwards the token to (e.g. Escurel) may key its tenant off this claim;
    /// unset → no `tenant` claim is added.
    pub static_upstream_tenant: Option<String>,
    /// #110: forward the resolved sender's `tenant` + `scope` on the minted
    /// static-upstream token (instead of `sub`-only + the deployment tenant).
    /// Default `false` — the default carriage contract is unchanged.
    pub static_upstream_forward_principal: bool,
    /// #114: allowlist of scopes that may be forwarded on the minted token
    /// (the `triton_sender_scopes` claim). Empty → no allowlist (caps only);
    /// non-empty → forwarded scopes are intersected with this set.
    pub static_upstream_scope_allowlist: Vec<String>,
    /// NFR-S-4: operator-configured DNS suffixes a `TRITON_STATIC_UPSTREAMS`
    /// hostname endpoint may end with and still pass the egress allowlist.
    /// Empty/unset → the strict default `[".ts.net"]` (Tailscale MagicDNS),
    /// so behaviour is unchanged unless an operator explicitly opts in to a
    /// trusted private split-DNS domain (e.g. `.int.data-zoo.de`). Entries
    /// are trimmed, blanks dropped, and compared case-insensitively. This
    /// only widens the hostname path — the IP-literal rules are unaffected.
    pub egress_allowed_suffixes: Vec<String>,
}

impl Settings {
    pub fn from_args() -> Self {
        Cli::parse().into()
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "triton",
    about = "Multi-protocol agent-ingress gateway for the DataZoo Hetzner substrate.",
    version
)]
struct Cli {
    /// Bind host shared by all listeners. Defaults to 0.0.0.0
    /// so Nomad's bridge networking can route inbound traffic.
    #[arg(long, env = "TRITON_HOST", default_value = "0.0.0.0")]
    host: IpAddr,

    /// MCP listener port. FR-A-1.
    #[arg(long, env = "TRITON_MCP_PORT", default_value_t = 8001)]
    mcp_port: u16,

    /// A2A listener port. FR-A-1.
    #[arg(long, env = "TRITON_A2A_PORT", default_value_t = 8002)]
    a2a_port: u16,

    /// REST listener port. FR-A-1.
    #[arg(long, env = "TRITON_REST_PORT", default_value_t = 8003)]
    rest_port: u16,

    /// Drain deadline in seconds for SIGTERM/SIGINT (FR-L-2). After
    /// this many seconds in-flight connections are aborted so the
    /// process can exit within Nomad's stop window.
    #[arg(long, env = "TRITON_DRAIN_DEADLINE_SECS", default_value_t = 30)]
    drain_deadline_secs: u64,

    /// Environment label used in audit lines and `/version`.
    #[arg(long, env = "TRITON_ENV", default_value = "local")]
    env: String,

    /// Golden-image SHA, normally injected by the substrate's Nomad
    /// template at deploy time. Optional; `/version` reports null
    /// when unset.
    #[arg(long, env = "TRITON_IMAGE_SHA")]
    image_sha: Option<String>,

    /// Substrate OIDC issuer URL. When set, every inbound bearer
    /// is verified against this issuer's JWKS (FR-I-1). When unset,
    /// the dev-token fallback is the only accepted identity (and
    /// only if the binary was built with `--features dev-token`).
    #[arg(long, env = "TRITON_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    /// Required `aud` claim. The substrate issues per-env audiences
    /// (e.g. `agents-nonprod`, `agents-prod`). Tokens carrying any
    /// other audience are rejected.
    #[arg(long, env = "TRITON_OIDC_AUDIENCE")]
    oidc_audience: Option<String>,

    /// Dedicated `aud` for the agent-initiated outbound surface
    /// (`POST /v1/outbound`, #95). Mirrors the `agent-oidc-swap`
    /// audience pattern in the other direction; required to enable the
    /// endpoint when OIDC is configured.
    #[arg(long, env = "TRITON_OUTBOUND_AUDIENCE")]
    outbound_audience: Option<String>,

    /// Dedicated issuer for the outbound surface (#100); falls back
    /// to `TRITON_OIDC_ISSUER` when unset. Set this to the upstream
    /// agent's own issuer URL for mirror-image static-upstream
    /// signing (the inverse of #83).
    #[arg(long, env = "TRITON_OUTBOUND_ISSUER")]
    outbound_issuer: Option<String>,

    /// Explicit JWKS URL for the outbound verifier (#100). When set,
    /// keys are fetched from this URL directly — the agent issuer
    /// need not serve an OIDC discovery endpoint.
    #[arg(long, env = "TRITON_OUTBOUND_JWKS_URL")]
    outbound_jwks_url: Option<String>,

    /// #115: per-tenant rate limit for `POST /v1/outbound`
    /// (messages/sec). Default 25. The global floor is 10× this.
    #[arg(long, env = "TRITON_OUTBOUND_RATE_LIMIT", default_value_t = 25)]
    outbound_rate_limit: u32,

    /// #115: per-tenant burst for `POST /v1/outbound`. Default 50.
    #[arg(long, env = "TRITON_OUTBOUND_RATE_LIMIT_BURST", default_value_t = 50)]
    outbound_rate_limit_burst: u32,

    /// Per-upstream-call timeout in milliseconds.
    #[arg(long, env = "TRITON_UPSTREAM_TIMEOUT_MS", default_value_t = 10_000)]
    upstream_timeout_ms: u64,

    /// Per-tool circuit-breaker open-after threshold (FR-U-3): consecutive
    /// tool-side faults before the breaker trips open.
    #[arg(long, env = "TRITON_CIRCUIT_OPEN_AFTER", default_value_t = 5)]
    circuit_open_after: u32,

    /// Per-tool circuit-breaker cooldown in milliseconds (FR-U-3) before a
    /// half-open probe is allowed through.
    #[arg(long, env = "TRITON_CIRCUIT_COOLDOWN_MS", default_value_t = 30_000)]
    circuit_cooldown_ms: u64,

    /// Bind host for the tailnet-only `/metrics` listener. Set to
    /// the Tailscale interface in production. Defaults to
    /// `127.0.0.1` for local-dev safety.
    #[arg(long, env = "TRITON_METRICS_HOST", default_value = "127.0.0.1")]
    metrics_host: IpAddr,

    /// Tailnet metrics port. When `0` the metrics listener is
    /// disabled entirely (useful when you don't want a `/metrics`
    /// surface at all).
    #[arg(long, env = "TRITON_METRICS_PORT", default_value_t = 9090)]
    metrics_port: u16,

    /// Optional v0.2 `adapter.yaml` manifest path. When set, the
    /// binary loads + closed-checks it at boot (FR-L-4..6); any
    /// validation failure refuses startup. When unset, Triton runs
    /// in v0.1 mode (HTTP trio only, no chat-channel adapters).
    #[arg(long, env = "TRITON_MANIFEST_PATH")]
    manifest_path: Option<std::path::PathBuf>,

    /// Bind host for the shared chat-channel webhook listener.
    /// Each manifest-declared webhook adapter mounts at
    /// `/<adapter-name>/webhook` on this listener.
    #[arg(long, env = "TRITON_CHAT_WEBHOOK_HOST", default_value = "127.0.0.1")]
    chat_webhook_host: IpAddr,

    /// Shared chat-channel webhook port. `0` disables the listener.
    #[arg(long, env = "TRITON_CHAT_WEBHOOK_PORT", default_value_t = 8004)]
    chat_webhook_port: u16,

    /// Base URL the Telegram outbound courier POSTs to. Production
    /// stays at `https://api.telegram.org`; integration tests
    /// override this to point at an in-repo `FakeTelegramApi`.
    #[arg(
        long,
        env = "TRITON_TELEGRAM_API_BASE",
        default_value = "https://api.telegram.org"
    )]
    telegram_api_base: String,

    /// Base URL the WhatsApp outbound courier POSTs to. Production
    /// stays at `https://graph.facebook.com` (Meta Graph API);
    /// integration tests override this to point at the in-repo
    /// `FakeWhatsAppApi`. NFR-S-4 egress allowlist refuses any
    /// non-canonical value outside `local` env.
    #[arg(
        long,
        env = "TRITON_WHATSAPP_API_BASE",
        default_value = "https://graph.facebook.com"
    )]
    whatsapp_api_base: String,

    /// Discord Gateway WebSocket URL the socket inbound connects to.
    /// Production stays at `wss://gateway.discord.gg`; integration
    /// tests override it to an in-repo fake gateway. NFR-S-4 egress
    /// allowlist refuses non-canonical values outside `local` env.
    #[arg(
        long,
        env = "TRITON_DISCORD_GATEWAY_URL",
        default_value = "wss://gateway.discord.gg"
    )]
    discord_gateway_url: String,

    /// Base URL the Discord Gateway adapter POSTs replies to (REST
    /// `/api/v10/channels/{id}/messages`). Production stays at
    /// `https://discord.com`; tests point it at the fake.
    #[arg(
        long,
        env = "TRITON_DISCORD_API_BASE",
        default_value = "https://discord.com"
    )]
    discord_api_base: String,

    /// Address of the local WhatsApp Web bridge daemon (Baileys-style
    /// sidecar) the socket inbound connects to: `tcp://host:port` or
    /// `unix:///path`. The bridge terminates the WhatsApp Web session
    /// inside the trust boundary; outside `local` env it MUST be a
    /// `unix://` path or a `tcp://*.ts.net` tailnet target (NFR-S-4,
    /// mirrors the Signal signald locality rule — loopback is allowed
    /// only in `local`). Empty disables the adapter.
    #[arg(long, env = "TRITON_WHATSAPP_BRIDGE_ADDR", default_value = "")]
    whatsapp_bridge_addr: String,

    /// Per-call timeout for outbound chat couriers
    /// (sendMessage and friends), in milliseconds.
    #[arg(long, env = "TRITON_COURIER_TIMEOUT_MS", default_value_t = 10_000)]
    courier_timeout_ms: u64,

    /// JWKS URI used by the Google Chat adapter to fetch the Google
    /// service-account certificates it verifies inbound JWTs
    /// against. Production stays at the canonical Google URL (the
    /// only host on the NFR-S-4 egress allowlist for this adapter).
    /// Integration tests override this to point at an in-repo
    /// `FakeGoogleJwks`.
    #[arg(
        long,
        env = "TRITON_GOOGLE_CHAT_JWKS_URI",
        default_value = "https://www.googleapis.com/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com"
    )]
    google_chat_jwks_uri: String,

    /// signald daemon address for the Signal adapter (PR 34).
    /// Format: `tcp://<host>:<port>` or `unix:///path/to/sock`.
    /// Empty default — `adapter.yaml` carries the manifest value.
    /// Outside `local` env the operator MUST set this per NFR-S-4
    /// egress allowlist, otherwise the binary refuses to wire the
    /// Signal adapter.
    #[arg(long, env = "TRITON_SIGNAL_SIGNALD_ADDR", default_value = "")]
    signal_signald_addr: String,

    /// OpenID discovery URL for the Microsoft Teams adapter. Default
    /// is the canonical Bot Framework endpoint. Outside `local` env
    /// the binary refuses any override (NFR-S-4 egress allowlist).
    #[arg(
        long,
        env = "TRITON_MSTEAMS_OPENID_URL",
        default_value = "https://login.botframework.com/v1/.well-known/openidconfiguration"
    )]
    msteams_openid_url: String,

    /// Optional override for Microsoft's OAuth2 token endpoint.
    /// `None` (the default) ⇒ the canonical hardcoded URL inside
    /// `token_client.rs`. Tests set this so the in-repo
    /// `FakeBotFramework` can mint stub access tokens.
    #[arg(long, env = "TRITON_MSTEAMS_TOKEN_URL")]
    msteams_token_url: Option<String>,

    /// PR 37: comma-separated `serviceUrl` host extras the MS Teams
    /// JWT verifier accepts. Empty by default; the binary refuses
    /// non-empty values outside `local` env.
    #[arg(
        long,
        env = "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS",
        default_value = ""
    )]
    msteams_extra_service_url_hosts: String,

    /// Comma-separated CORS allow-list (e.g.
    /// `https://explorer-nonprod.tailnet.ts.net`). Default empty:
    /// no CORS layer mounted, response headers identical to v0.1.
    /// Set this to enable browser frontends like the Flutter
    /// explorer at `apps/explorer/`.
    #[arg(long, env = "TRITON_CORS_ALLOWED_ORIGINS", default_value = "")]
    cors_allowed_origins: String,

    /// OIDC `client_id` for the Flutter explorer SPA. Reported at
    /// `/v1/runtime` so the SPA can self-bootstrap PKCE without
    /// baked-in config. Unset ⇒ the explorer isn't operator-enabled
    /// in this env.
    #[arg(long, env = "TRITON_EXPLORER_CLIENT_ID")]
    explorer_client_id: Option<String>,

    /// Trust `X-Forwarded-Email` from a co-located oauth2-proxy
    /// sidecar instead of requiring an `Authorization: Bearer` on
    /// every request. Default `false`. Issue #67. ONLY safe when
    /// Triton binds loopback inside a Nomad alloc so the only thing
    /// that can set the header is the sidecar in the shared netns.
    #[arg(long, env = "TRITON_TRUST_FORWARDED_AUTH", default_value_t = false)]
    trust_forwarded_auth: bool,

    /// Serve the whole HTTP trio on the single REST port (REST at root,
    /// MCP at `/mcp`, A2A at `/a2a`) instead of three separate ports.
    /// Accepts `true`/`1` (case-insensitive) as on; anything else is off.
    /// Default off — three-port behavior is unchanged.
    #[arg(long, env = "TRITON_SINGLE_PORT", default_value = "false")]
    single_port: String,

    /// PR 36: dashboard rasterizer service URL (FR-A-11).
    /// Default points at the in-repo `triton-rasterizer` binary
    /// running on its 12-factor default port. Outside `local` env
    /// the operator MUST point this at a tailnet-resolved host.
    #[arg(
        long,
        env = "TRITON_RASTERIZER_URL",
        default_value = "http://127.0.0.1:9320"
    )]
    rasterizer_url: String,

    /// RSA private key signing static-upstream JWTs (workload→workload auth
    /// without Vault): a raw PEM, or base64-encoded PEM (so a multi-line key
    /// rides a single env var / Kamal env-file line). With
    /// `--static-upstream-issuer` and `--jwt-jwks`, Triton mints a per-call
    /// OIDC token instead of the static bearer and serves JWKS.
    #[arg(long, env = "TRITON_JWT_SIGNING_KEY")]
    jwt_signing_key: Option<String>,

    /// Public JWKS JSON matching `--jwt-signing-key` (same `kid`), served at
    /// `/.well-known/jwks.json`.
    #[arg(long, env = "TRITON_JWT_JWKS")]
    jwt_jwks: Option<String>,

    /// `kid` for minted token headers; must match the key in `--jwt-jwks`.
    #[arg(long, env = "TRITON_JWT_KID")]
    jwt_kid: Option<String>,

    /// Issuer URL Triton advertises for the JWTs it mints (token `iss` +
    /// discovery `issuer`). Agents set `AGENT_OIDC_ISSUER` to this.
    #[arg(long, env = "TRITON_SELF_ISSUER")]
    static_upstream_issuer: Option<String>,

    /// `aud` for minted static-upstream tokens (the agent's expected audience).
    /// Defaults to `agents-<env>` when unset.
    #[arg(long, env = "TRITON_STATIC_UPSTREAM_AUD")]
    static_upstream_aud: Option<String>,

    /// `tenant` claim for minted static-upstream tokens (a forwarded-to
    /// downstream like Escurel may key its tenant off it). Unset → no claim.
    #[arg(long, env = "TRITON_STATIC_UPSTREAM_TENANT")]
    static_upstream_tenant: Option<String>,

    /// #110: forward the resolved sender's `tenant` + `scope` on the minted
    /// static-upstream token instead of `sub`-only. Default off so the
    /// default upstream-agent contract is unchanged.
    #[arg(
        long,
        env = "TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL",
        default_value_t = false
    )]
    static_upstream_forward_principal: bool,

    /// #114: comma-separated allowlist of scopes that may be forwarded on
    /// the minted token. Empty → no allowlist (caps only).
    #[arg(
        long,
        env = "TRITON_STATIC_UPSTREAM_SCOPE_ALLOWLIST",
        default_value = ""
    )]
    static_upstream_scope_allowlist: String,

    /// NFR-S-4: comma-separated DNS suffixes the static-upstream egress
    /// allowlist trusts for HOSTNAME endpoints (e.g.
    /// `.ts.net,.int.data-zoo.de`). Empty/unset → the strict default
    /// `.ts.net` only. An explicit operator opt-in for a private or
    /// tailnet-backed domain (the substrate's split-DNS `*.int.data-zoo.de`
    /// resolves to private host IPs). Does NOT relax the IP-literal rules
    /// and performs no DNS resolution.
    #[arg(long, env = "TRITON_EGRESS_ALLOWED_SUFFIXES", default_value = "")]
    egress_allowed_suffixes: String,
}

impl From<Cli> for Settings {
    fn from(c: Cli) -> Self {
        Self {
            host: c.host,
            mcp_port: c.mcp_port,
            a2a_port: c.a2a_port,
            rest_port: c.rest_port,
            drain_deadline: Duration::from_secs(c.drain_deadline_secs),
            env: c.env,
            image_sha: c.image_sha,
            oidc_issuer: c.oidc_issuer,
            oidc_audience: c.oidc_audience,
            outbound_audience: c.outbound_audience,
            outbound_issuer: c.outbound_issuer,
            outbound_jwks_url: c.outbound_jwks_url,
            outbound_rate_limit_per_sec: c.outbound_rate_limit,
            outbound_rate_limit_burst: c.outbound_rate_limit_burst,
            static_upstream_tenant: c.static_upstream_tenant,
            static_upstream_forward_principal: c.static_upstream_forward_principal,
            static_upstream_scope_allowlist: c
                .static_upstream_scope_allowlist
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            egress_allowed_suffixes: parse_egress_suffixes(&c.egress_allowed_suffixes),
            upstream_timeout: Duration::from_millis(c.upstream_timeout_ms),
            circuit_open_after: c.circuit_open_after,
            circuit_cooldown: Duration::from_millis(c.circuit_cooldown_ms),
            metrics_host: c.metrics_host,
            metrics_port: c.metrics_port,
            manifest_path: c.manifest_path,
            chat_webhook_host: c.chat_webhook_host,
            chat_webhook_port: c.chat_webhook_port,
            telegram_api_base: c.telegram_api_base,
            whatsapp_api_base: c.whatsapp_api_base,
            discord_gateway_url: c.discord_gateway_url,
            discord_api_base: c.discord_api_base,
            whatsapp_bridge_addr: c.whatsapp_bridge_addr,
            google_chat_jwks_uri: c.google_chat_jwks_uri,
            signal_signald_addr: c.signal_signald_addr,
            msteams_openid_url: c.msteams_openid_url,
            msteams_token_url: c.msteams_token_url,
            msteams_extra_service_url_hosts: c
                .msteams_extra_service_url_hosts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            courier_timeout: Duration::from_millis(c.courier_timeout_ms),
            cors_allowed_origins: triton_adapters_http::cors::parse_origins(
                &c.cors_allowed_origins,
            ),
            explorer_client_id: c.explorer_client_id,
            rasterizer_url: c.rasterizer_url,
            trust_forwarded_auth: c.trust_forwarded_auth,
            single_port: matches!(
                c.single_port.trim().to_ascii_lowercase().as_str(),
                "true" | "1"
            ),
            jwt_signing_key: c.jwt_signing_key,
            jwt_jwks: c.jwt_jwks,
            jwt_kid: c.jwt_kid,
            static_upstream_issuer: c.static_upstream_issuer,
            static_upstream_aud: c.static_upstream_aud,
        }
    }
}

/// Parse `TRITON_EGRESS_ALLOWED_SUFFIXES` into the NFR-S-4 hostname egress
/// allowlist. Comma-separated; entries are trimmed and blanks dropped. An
/// empty/blank-only input falls back to the strict default `[".ts.net"]`, so
/// the egress policy is unchanged unless an operator explicitly opts in.
/// Lowercasing for comparison happens in the guard (`endpoint_is_dispatchable`),
/// so the configured values are reported verbatim in the boot audit line.
fn parse_egress_suffixes(raw: &str) -> Vec<String> {
    let parsed: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if parsed.is_empty() {
        vec![".ts.net".to_string()]
    } else {
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::parse_egress_suffixes;

    #[test]
    fn egress_suffixes_default_to_tailnet_when_empty() {
        assert_eq!(parse_egress_suffixes(""), vec![".ts.net".to_string()]);
        // Whitespace / blank-only input is still the strict default.
        assert_eq!(parse_egress_suffixes("   "), vec![".ts.net".to_string()]);
        assert_eq!(parse_egress_suffixes(" , ,"), vec![".ts.net".to_string()]);
    }

    #[test]
    fn egress_suffixes_parse_trim_and_drop_blanks() {
        assert_eq!(
            parse_egress_suffixes(".ts.net, .int.data-zoo.de ,"),
            vec![".ts.net".to_string(), ".int.data-zoo.de".to_string()],
        );
        // A single explicit suffix replaces the default entirely.
        assert_eq!(
            parse_egress_suffixes(".int.data-zoo.de"),
            vec![".int.data-zoo.de".to_string()],
        );
    }
}
