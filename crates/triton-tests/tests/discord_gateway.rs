//! v0.2 — Discord Gateway socket inbound (FR-A-1.v0.2, M-LIFECYCLE-1).
//!
//! Drives the gateway worker against a real fake Gateway WebSocket
//! server (no mocks): HELLO → IDENTIFY → READY → MESSAGE_CREATE is
//! dispatched through the same pipeline as the Interactions webhook,
//! and the reply is POSTed to the fake's REST endpoint. A forced
//! disconnect exercises the bounded-reconnect lifecycle: the worker
//! reconnects (connection count 1 → 2) and resumes dispatching.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::discord_gateway_fixture::FakeDiscordGateway;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-discord-gateway-test.yaml")
        .display()
        .to_string()
}

fn message_create(author_id: &str, channel_id: &str, content: &str) -> Value {
    json!({
        "id": "m1",
        "channel_id": channel_id,
        "author": { "id": author_id, "bot": false, "username": "Alice" },
        "content": content,
        "timestamp": "2026-05-25T10:00:00.000000+00:00"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_dispatches_message_and_recovers_after_disconnect() {
    // First connection emits one message; after the forced reconnect
    // the second connection emits another.
    let gateway = FakeDiscordGateway::start(vec![
        message_create("7777", "chan-1", "echo via gateway"),
        message_create("7777", "chan-1", "echo after reconnect"),
    ])
    .await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        (
            "TRITON_DISCORD_GATEWAY_URL".to_string(),
            gateway.gateway_url(),
        ),
        ("TRITON_DISCORD_API_BASE".to_string(), gateway.rest_base()),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // 1. The gateway worker connected, identified, received the
    //    MESSAGE_CREATE, and dispatched it like any other inbound.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:discord"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["who"], "alice");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    // 2. The reply was POSTed back to Discord's REST with the bot
    //    token (never logged; asserted here on the captured header).
    let replies = wait_for_async(Duration::from_secs(5), || async {
        let r = gateway.captured_replies().await;
        (!r.is_empty()).then_some(r)
    })
    .await;
    assert_eq!(replies[0].channel_id, "chan-1");
    assert_eq!(replies[0].authorization, "Bot bot-token-for-test");
    assert!(
        replies[0].body["content"]
            .as_str()
            .unwrap_or("")
            .contains("echo via gateway")
    );

    // 3. M-LIFECYCLE-1: force a disconnect; the worker reconnects
    //    within the bounded budget and resumes dispatching.
    gateway.force_disconnect();

    wait_for_async(Duration::from_secs(10), || async {
        (gateway.connection_count() >= 2).then_some(())
    })
    .await;

    // The post-reconnect message is dispatched + replied.
    wait_for_async(Duration::from_secs(5), || async {
        let r = gateway.captured_replies().await;
        r.iter()
            .any(|c| {
                c.body["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("echo after reconnect")
            })
            .then_some(())
    })
    .await;

    // Prove it actually RESUMEd (not re-IDENTIFYed): the second
    // connection presented op 6 RESUME with the session id from the
    // first connection's READY.
    let modes = gateway.connection_modes().await;
    assert_eq!(modes.first().map(String::as_str), Some("identify"));
    assert_eq!(
        modes.get(1).map(String::as_str),
        Some("resume"),
        "second connection must RESUME, not re-IDENTIFY; modes={modes:?}"
    );
    assert_eq!(
        gateway
            .resumed_session_ids()
            .await
            .first()
            .map(String::as_str),
        Some("sess-abc"),
        "RESUME must carry the session id from the first READY"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_reconnects_when_heartbeats_unacked() {
    // M-LIFECYCLE-1 zombie detection: the server advertises a short
    // heartbeat interval and NEVER ACKs. With no forced disconnect,
    // the client must notice the missing ACK and reconnect on its own.
    let gateway =
        FakeDiscordGateway::start_zombie(vec![message_create("7777", "chan-1", "zombie probe")])
            .await;

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        (
            "TRITON_DISCORD_GATEWAY_URL".to_string(),
            gateway.gateway_url(),
        ),
        ("TRITON_DISCORD_API_BASE".to_string(), gateway.rest_base()),
    ]);
    let _proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // No force_disconnect() here: the only way the connection count
    // climbs is the client detecting the un-ACKed heartbeat and
    // reconnecting itself.
    wait_for_async(Duration::from_secs(10), || async {
        (gateway.connection_count() >= 2).then_some(())
    })
    .await;
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

async fn wait_for_async<T, F, Fut>(deadline: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let start = Instant::now();
    loop {
        if let Some(v) = probe().await {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe did not return Some within {deadline:?}");
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
