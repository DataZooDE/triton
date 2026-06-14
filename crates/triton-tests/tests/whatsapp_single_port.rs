//! Single-port mode serves the chat webhook on the unified HTTP port.
//!
//! The substrate model is one port per host (REST/MCP/A2A path-multiplexed
//! behind kamal-proxy). For an external WhatsApp webhook to ride kamal-
//! proxy's TLS on that one public host, the webhook must be on the SAME
//! port — not the separate `chat_webhook_port` listener. With
//! TRITON_SINGLE_PORT=true the `/whatsapp/webhook` route is mounted on the
//! REST listener, so Meta's verification GET resolves there.
//!
//! No mocks: real binary, real HTTP, real manifest.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;

const VERIFY_TOKEN: &str = "meta-verify-token-for-test";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-test.yaml")
        .display()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_is_served_on_the_single_http_port() {
    let whatsapp = FakeWhatsAppApi::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // Meta's subscription handshake on the SINGLE (REST) port — not the
    // separate chat-webhook port. A matching verify_token echoes the
    // challenge; if the webhook weren't mounted here this would 404.
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/whatsapp/webhook"))
        .query(&[
            ("hub.mode", "subscribe"),
            ("hub.verify_token", VERIFY_TOKEN),
            ("hub.challenge", "single-port-challenge"),
        ])
        .send()
        .await
        .expect("GET verify on the single port");
    assert_eq!(
        resp.status(),
        200,
        "webhook must answer on the unified HTTP port in single_port mode"
    );
    assert_eq!(resp.text().await.expect("body"), "single-port-challenge");
}
