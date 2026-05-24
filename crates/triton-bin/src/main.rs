//! `triton` — multi-protocol agent-ingress gateway. See `doc/`.
//!
//! Walking-skeleton scope (PR 1): bind a single REST listener and
//! serve `/healthz`. PR 2 adds the MCP and A2A listeners under
//! `tokio::select!` plus SIGTERM-drain. PR 3 adds 12-factor settings
//! and `/version`. Everything else lives in later PRs per `CLAUDE.md`.

use std::net::SocketAddr;

use tokio::net::TcpListener;

mod settings;
use settings::Settings;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();

    let settings = Settings::from_env();
    let rest_addr: SocketAddr = (settings.host, settings.rest_port).into();
    tracing::info!(addr = %rest_addr, "triton: binding REST listener");

    let listener = TcpListener::bind(rest_addr).await?;
    let app = triton_adapters_http::rest::router();
    axum::serve(listener, app).await
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
