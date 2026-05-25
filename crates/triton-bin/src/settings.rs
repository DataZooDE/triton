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
    pub consul_url: Option<String>,
    pub vault_url: Option<String>,
    pub vault_token: Option<String>,
    pub vault_oidc_role: String,
    pub circuit_open_after: u32,
    pub circuit_cooldown: Duration,
    pub upstream_timeout: Duration,
    pub metrics_host: IpAddr,
    pub metrics_port: u16,
    pub manifest_path: Option<std::path::PathBuf>,
    pub chat_webhook_host: IpAddr,
    pub chat_webhook_port: u16,
    pub telegram_api_base: String,
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
    /// can mint stub access tokens.
    pub msteams_token_url: Option<String>,
    pub courier_timeout: Duration,
    /// Comma-separated allow-list of origins that may make
    /// cross-origin requests to the HTTP trio. Empty by default —
    /// in that case no CORS layer is mounted at all (production
    /// parity for substrate deployments that don't need a browser
    /// frontend). Set in nonprod to enable the Flutter explorer.
    pub cors_allowed_origins: Vec<String>,
    /// OIDC `client_id` the Flutter explorer should use for its PKCE
    /// login. Reported at `GET /v1/runtime` so the SPA doesn't bake
    /// it in. `None` ⇒ the operator hasn't enabled the explorer in
    /// this env; the discovery endpoint returns null and the SPA
    /// shows an "ask an operator" message.
    pub explorer_client_id: Option<String>,
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

    /// Consul HTTP base URL (e.g. `http://127.0.0.1:8500`). When
    /// unset the upstream router is disabled and the dispatcher
    /// only serves in-process tools.
    #[arg(long, env = "TRITON_CONSUL_URL")]
    consul_url: Option<String>,

    /// Vault HTTP base URL.
    #[arg(long, env = "TRITON_VAULT_URL")]
    vault_url: Option<String>,

    /// Triton's own Vault auth token (Nomad-templated at deploy
    /// time). Used to mint per-call agent OIDC tokens.
    #[arg(long, env = "TRITON_VAULT_TOKEN")]
    vault_token: Option<String>,

    /// Vault OIDC role for the per-call swap (FR-U-2).
    #[arg(
        long,
        env = "TRITON_VAULT_OIDC_ROLE",
        default_value = "agent-oidc-swap"
    )]
    vault_oidc_role: String,

    /// Per-tool circuit-breaker open-after threshold (FR-U-3).
    #[arg(long, env = "TRITON_CIRCUIT_OPEN_AFTER", default_value_t = 5)]
    circuit_open_after: u32,

    /// Per-tool circuit-breaker cooldown in milliseconds (FR-U-3).
    #[arg(long, env = "TRITON_CIRCUIT_COOLDOWN_MS", default_value_t = 30_000)]
    circuit_cooldown_ms: u64,

    /// Per-upstream-call timeout in milliseconds.
    #[arg(long, env = "TRITON_UPSTREAM_TIMEOUT_MS", default_value_t = 10_000)]
    upstream_timeout_ms: u64,

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
            consul_url: c.consul_url,
            vault_url: c.vault_url,
            vault_token: c.vault_token,
            vault_oidc_role: c.vault_oidc_role,
            circuit_open_after: c.circuit_open_after,
            circuit_cooldown: Duration::from_millis(c.circuit_cooldown_ms),
            upstream_timeout: Duration::from_millis(c.upstream_timeout_ms),
            metrics_host: c.metrics_host,
            metrics_port: c.metrics_port,
            manifest_path: c.manifest_path,
            chat_webhook_host: c.chat_webhook_host,
            chat_webhook_port: c.chat_webhook_port,
            telegram_api_base: c.telegram_api_base,
            google_chat_jwks_uri: c.google_chat_jwks_uri,
            signal_signald_addr: c.signal_signald_addr,
            msteams_openid_url: c.msteams_openid_url,
            msteams_token_url: c.msteams_token_url,
            courier_timeout: Duration::from_millis(c.courier_timeout_ms),
            cors_allowed_origins: triton_adapters_http::cors::parse_origins(
                &c.cors_allowed_origins,
            ),
            explorer_client_id: c.explorer_client_id,
        }
    }
}
