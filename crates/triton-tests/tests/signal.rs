//! v0.2 PR 34 — Signal adapter (signald socket) integration tests.
//!
//! Proves the adapter end-to-end against a real TCP listener
//! speaking the signald wire protocol — no mocks, real binary,
//! real JSON lines.
//!
//! Five scenarios:
//!  1. Connect-and-subscribe on boot.
//!  2. IncomingMessage dispatches a tool and the reply ships back
//!     as a signald `send` line, with the expected audit shape.
//!  3. Unknown sender is silently dropped with a `phase: rejected`
//!     audit line tagged `result: error:auth`.
//!  4. Empty-body messages produce no send line and no audit lines.
//!  5. After signald drops the connection the adapter reconnects
//!     and re-issues `subscribe`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use triton_tests::TritonProcess;
use triton_tests::signald_fixture::FakeSignald;

const BOT_ACCOUNT: &str = "+15551234567";
const KNOWN_UUID: &str = "00000000-0000-0000-0000-000000000001";
const KNOWN_NUMBER: &str = "+15559999999";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-signal-test.yaml")
        .display()
        .to_string()
}

fn env_with_signald(uri: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_SIGNAL_SIGNALD_ADDR".to_string(), uri.to_string()),
    ])
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_and_subscribes_on_boot() {
    let signald = FakeSignald::start().await;
    let uri = signald.tcp_uri();
    let _proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_signald(&uri)).await;

    let line = signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("subscribe within 5s");
    assert_eq!(line.value["account"], BOT_ACCOUNT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_message_dispatches_and_sends_reply() {
    let signald = FakeSignald::start().await;
    let uri = signald.tcp_uri();
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_signald(&uri)).await;

    // Wait for the adapter to subscribe before pushing a message —
    // signald only streams events to a subscribed client.
    let _ = signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("subscribe");

    // Push an IncomingMessage from the known sender with `/echo`.
    signald.emit_incoming(KNOWN_UUID, Some(KNOWN_NUMBER), "/echo hello world");

    // The adapter dispatches `echo` (one-field object → text path).
    let audit = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:signal"
    });
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["tenant"], "acme");
    assert_eq!(audit["result"], "ok");

    // And the courier shipped a `send` line to signald with the
    // rendered body. Echo returns `{ "echo": "<message>" }` →
    // mapper falls back to bare text.
    let send_line = signald
        .wait_for_type("send", Duration::from_secs(3))
        .await
        .expect("send line");
    assert_eq!(send_line.value["username"], BOT_ACCOUNT);
    assert_eq!(send_line.value["recipientAddress"]["uuid"], KNOWN_UUID);
    assert_eq!(send_line.value["recipientAddress"]["number"], KNOWN_NUMBER);
    let body = send_line.value["messageBody"]
        .as_str()
        .expect("messageBody str");
    assert!(
        body.contains("hello world"),
        "expected reply to include `hello world`; got {body:?}",
    );

    // And a `phase: post` audit line should follow the send.
    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:signal"
    });
    assert_eq!(post_audit["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_dropped_with_audit() {
    let signald = FakeSignald::start().await;
    let uri = signald.tcp_uri();
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_signald(&uri)).await;
    let _ = signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("subscribe");

    // UUID NOT in sender_table.
    let bogus_uuid = "11111111-1111-1111-1111-111111111111";
    signald.emit_incoming(bogus_uuid, None, "/echo trespass");

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:signal"
    });
    assert_eq!(rejected["result"], "error:auth");

    // No send line should appear within a small wait window — Signal
    // is a non-HTTP transport, the unknown-sender path is a silent
    // drop on the wire.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        signald.count_with_type("send"),
        0,
        "expected no `send` line for unknown sender",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_body_messages_silently_skipped() {
    let signald = FakeSignald::start().await;
    let uri = signald.tcp_uri();
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_signald(&uri)).await;
    let _ = signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("subscribe");

    // Drain audit count before the test event so the post-event
    // check sees only audit lines emitted AFTER the empty-body
    // event arrives.
    let before = proc.stdout_snapshot().len();
    signald.emit_incoming(KNOWN_UUID, Some(KNOWN_NUMBER), "");

    // No audit line should fire — empty bodies are receipts /
    // typing indicators and the adapter swallows them silently.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = proc.stdout_snapshot();
    let new_lines: Vec<_> = after.into_iter().skip(before).collect();
    for line in &new_lines {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v["kind"] == "audit"
            && v["protocol"] == "messenger:signal"
        {
            panic!(
                "did not expect a messenger:signal audit line for an empty-body event; got: {line}"
            );
        }
    }
    assert_eq!(
        signald.count_with_type("send"),
        0,
        "expected no `send` line for empty-body event",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_signald_drops_connection() {
    let signald = FakeSignald::start().await;
    let uri = signald.tcp_uri();
    let _proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_signald(&uri)).await;

    // First subscribe lands quickly after boot.
    let _ = signald
        .wait_for_type("subscribe", Duration::from_secs(5))
        .await
        .expect("first subscribe");
    assert_eq!(signald.connections(), 1);

    // Force-close. Adapter should reconnect within ~5s
    // (initial backoff 500ms; we give some slack for accept
    // race + reconnect handshake).
    signald.force_disconnect();

    // Wait for a SECOND subscribe — implies the adapter
    // reconnected and re-issued it.
    let start = Instant::now();
    let deadline = Duration::from_secs(8);
    loop {
        if signald.count_with_type("subscribe") >= 2 {
            break;
        }
        if start.elapsed() > deadline {
            panic!(
                "expected 2 subscribe lines after reconnect; got {} (connections={})",
                signald.count_with_type("subscribe"),
                signald.connections()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(signald.connections() >= 2);
}
