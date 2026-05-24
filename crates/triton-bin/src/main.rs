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
use triton_adapters_http::rest::RestState;
use triton_core::{Dispatcher, RuntimeInfo, ToolRegistry};

mod settings;
mod tools;
use settings::Settings;

const BINARY_SHA: &str = env!("TRITON_BUILD_SHA");

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();
    let settings = Arc::new(Settings::from_args());

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mcp_addr: SocketAddr = (settings.host, settings.mcp_port).into();
    let a2a_addr: SocketAddr = (settings.host, settings.a2a_port).into();
    let rest_addr: SocketAddr = (settings.host, settings.rest_port).into();

    let mcp_listener = TcpListener::bind(mcp_addr).await?;
    let a2a_listener = TcpListener::bind(a2a_addr).await?;
    let rest_listener = TcpListener::bind(rest_addr).await?;
    tracing::info!(
        mcp = %mcp_addr,
        a2a = %a2a_addr,
        rest = %rest_addr,
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

    let registry = Arc::new(build_registry());
    let dispatcher = Arc::new(Dispatcher::new(registry, settings.env.clone()));

    let rest_state = RestState {
        runtime: runtime.clone(),
        dispatcher: dispatcher.clone(),
    };
    let a2a_state = A2aState {
        dispatcher: dispatcher.clone(),
        tasks: InMemoryTaskStore::new(),
    };

    let serve_mcp = axum::serve(mcp_listener, triton_adapters_http::mcp::router())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_a2a = axum::serve(a2a_listener, triton_adapters_http::a2a::router(a2a_state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_rest = axum::serve(
        rest_listener,
        triton_adapters_http::rest::router(rest_state),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned());

    let drain = async {
        let (a, b, c) = tokio::join!(serve_mcp, serve_a2a, serve_rest);
        a.and(b).and(c)
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
