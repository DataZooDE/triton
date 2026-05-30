//! Standalone fake Consul for the local browser/e2e demo.
//!
//! The same `upstream_fixture::FakeConsul` the integration tests use,
//! wrapped as a tiny binary so a shell harness (deploy/local-e2e) can
//! spawn it. It binds a random loopback port, prints its URL on the
//! first stdout line (so the caller can point Triton's
//! `TRITON_CONSUL_URL` at it), then parks.
//!
//! Usage:  fake-consul <name>=<host:port> [<name>=<host:port> ...]
//! e.g.    fake-consul hello=127.0.0.1:8090

use triton_tests::upstream_fixture::FakeConsul;

#[tokio::main]
async fn main() {
    let pairs: Vec<(String, String)> = std::env::args()
        .skip(1)
        .map(|a| {
            let (name, host_port) = a
                .split_once('=')
                .expect("expected <name>=<host:port> arguments");
            (name.to_string(), host_port.to_string())
        })
        .collect();
    assert!(!pairs.is_empty(), "fake-consul: need at least one service");

    let services: Vec<(&str, String)> = pairs
        .iter()
        .map(|(n, hp)| (n.as_str(), hp.clone()))
        .collect();
    let consul = FakeConsul::start(&services).await;
    println!("{}", consul.url());

    // Keep the spawned axum server alive.
    std::future::pending::<()>().await;
}
