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
        }
    }
}
