//! `triton` — multi-protocol agent-ingress gateway. See `doc/`.
//!
//! PR 2 scope: bind the MCP, A2A, REST listeners (ADR-1, FR-A-1) and
//! drive a graceful drain on SIGTERM/SIGINT (G-4, FR-L-2, FR-L-3).
//! Drain semantics: stop accepting new connections, let in-flight
//! complete or hit `drain_deadline`, flush stdout, exit 0.

use std::io::Write;
use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

mod settings;
use settings::Settings;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();
    let settings = Settings::from_env();

    // Install signal handlers BEFORE any await on serve, so a
    // SIGTERM that arrives during startup is captured rather than
    // taking the default termination action. `signal(...)` registers
    // the handler at construction; `.recv().await` happens later in
    // the spawned task.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mcp_addr: SocketAddr = (settings.host, settings.mcp_port).into();
    let a2a_addr: SocketAddr = (settings.host, settings.a2a_port).into();
    let rest_addr: SocketAddr = (settings.host, settings.rest_port).into();

    // Bind all three listeners up front. Any failure is fatal:
    // process exits non-zero, Nomad reschedules (FR-L-1, ADR-1).
    let mcp_listener = TcpListener::bind(mcp_addr).await?;
    let a2a_listener = TcpListener::bind(a2a_addr).await?;
    let rest_listener = TcpListener::bind(rest_addr).await?;
    tracing::info!(
        mcp = %mcp_addr,
        a2a = %a2a_addr,
        rest = %rest_addr,
        drain_deadline_secs = settings.drain_deadline.as_secs(),
        "triton: listeners bound",
    );

    // Single shutdown token; cloned into each serve and into the
    // signal-handler task. Cancelling it triggers
    // `with_graceful_shutdown` on every serve simultaneously.
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

    let serve_mcp = axum::serve(mcp_listener, triton_adapters_http::mcp::router())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_a2a = axum::serve(a2a_listener, triton_adapters_http::a2a::router())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let serve_rest = axum::serve(rest_listener, triton_adapters_http::rest::router())
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());

    // `tokio::join!` (not `try_join!`) so a failure in one serve
    // does not drop the siblings before they have a chance to drain.
    // The whole join is bounded by `drain_deadline` — FR-L-2
    // explicitly caps in-flight requests at a per-request deadline
    // so a stuck connection cannot keep the alloc alive past
    // Nomad's stop window.
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

    // Make sure the signal task is reaped even if it never fired
    // (e.g. drain was triggered by a serve failure).
    shutdown.cancel();
    let _ = signal_task.await;

    // FR-L-2: flush stdout before exit so the substrate audit
    // collector sees every line we emitted during drain.
    let _ = std::io::stdout().flush();

    tracing::info!("triton: exit");
    serve_result
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
