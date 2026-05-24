//! ACC-8 (cold start) — fresh process boots, all three HTTP
//! listeners bind, and REST `/healthz` returns 200. Per
//! `architecture.md` §5.2 `/healthz` lives on the REST listener only;
//! MCP and A2A liveness is checked by TCP connect.
//!
//! No mocks: real binary, real signals, real sockets.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_ok_and_all_three_listeners_bound() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("GET /healthz")
        .json()
        .await
        .expect("decode JSON");
    assert_eq!(body["status"], "ok", "unexpected body: {body}");

    // MCP and A2A listeners should be accepting connections by now;
    // their JSON-RPC / `POST /message:send` semantics land in later
    // PRs. A successful TCP connect is the cheapest honest check.
    for addr in [proc.mcp_addr, proc.a2a_addr] {
        tokio::net::TcpStream::connect(addr)
            .await
            .unwrap_or_else(|e| panic!("TCP connect to {addr} failed: {e}"));
    }
}
