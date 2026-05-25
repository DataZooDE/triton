//! v0.2 — WhatsApp Web bridge socket inbound (FR-A-1.v0.2,
//! M-LIFECYCLE-1).
//!
//! Drives the bridge worker against a real fake bridge daemon (no
//! mocks): the daemon emits a `message` line, the worker dispatches
//! it through the shared pipeline and writes a `send` reply back over
//! the socket. A forced disconnect exercises the bounded-reconnect
//! lifecycle — the worker reconnects (connection count 1 → 2) and
//! resumes dispatching with no operator intervention.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::whatsapp_bridge_fixture::FakeWhatsAppBridge;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-bridge-test.yaml")
        .display()
        .to_string()
}

fn message(from: &str, text: &str) -> Value {
    json!({ "type": "message", "from": from, "text": text })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_dispatches_message_and_recovers_after_disconnect() {
    let bridge = FakeWhatsAppBridge::start(vec![
        message("4915112345678", "echo via bridge"),
        message("4915112345678", "echo after reconnect"),
    ])
    .await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_BRIDGE_ADDR".to_string(), bridge.tcp_uri()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // 1. The worker connected, read the inbound message, and
    //    dispatched it like any other inbound.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:whatsapp"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    // 2. The reply was written back over the bridge socket as a
    //    `send` line addressed to the inbound sender.
    let send = wait_for(Duration::from_secs(5), || {
        bridge
            .captured()
            .into_iter()
            .find(|v| v.get("type").and_then(Value::as_str) == Some("send"))
    });
    assert_eq!(send["to"], "4915112345678");
    assert!(
        send["text"]
            .as_str()
            .unwrap_or("")
            .contains("echo via bridge"),
        "reply should echo the inbound text; got: {send}"
    );

    // 3. M-LIFECYCLE-1: force a disconnect; the worker reconnects and
    //    resumes dispatching with no operator intervention.
    bridge.force_disconnect();

    wait_for(Duration::from_secs(10), || {
        (bridge.connection_count() >= 2).then_some(())
    });

    // The post-reconnect message is dispatched + replied.
    wait_for(Duration::from_secs(5), || {
        bridge
            .captured()
            .into_iter()
            .any(|v| {
                v.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .contains("echo after reconnect")
            })
            .then_some(())
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_refuses_mixed_transport_shape() {
    // Codex review: a socket-inbound whatsapp adapter with a
    // rest_api outbound is a misconfiguration — booting as a bridge
    // would silently ignore the declared REST outbound. The adapter
    // MUST refuse to build (exit 2).
    let bin = locate_triton_binary();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-bridge-badshape.yaml")
        .display()
        .to_string();
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest)
        .env("TRITON_WHATSAPP_BRIDGE_ADDR", "tcp://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "mixed-shape whatsapp bridge adapter MUST refuse to boot; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let cand = here.join("target/debug/triton");
        if cand.exists() {
            return cand;
        }
        here.pop();
    }
    panic!("triton binary not found");
}

fn wait_for_audit<F>(proc: &TritonProcess, deadline: Duration, mut matches: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if matches(&v) {
                return v;
            }
        }
        if start.elapsed() > deadline {
            panic!(
                "audit line not found within {deadline:?}\nstdout:\n{}",
                proc.stdout_snapshot().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe did not return Some within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
