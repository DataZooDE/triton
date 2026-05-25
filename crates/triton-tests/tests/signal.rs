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

fn vault_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-signal-vault.yaml")
        .display()
        .to_string()
}

fn locate_triton_binary() -> PathBuf {
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

/// Spawn `triton` synchronously with the given env, collect the exit
/// status. Used by the NFR-S-4 boot-rejection tests where we expect
/// the binary to exit with code 2 BEFORE listeners come up.
fn spawn_and_wait_for_exit(env: &[(&str, &str)]) -> std::process::Output {
    let bin = locate_triton_binary();
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_rejects_non_tailnet_addr_in_nonprod() {
    // PR 37 Finding 7 (MEDIUM, NFR-S-4): the previous check only
    // refused an EMPTY TRITON_SIGNAL_SIGNALD_ADDR outside `local`.
    // Setting it to anything non-empty was accepted, so an operator
    // (or a compromised env var) could redirect signald connections
    // at an arbitrary host. The fix: outside `local`, the tcp://
    // host MUST end with `.ts.net` (the Tailscale tailnet domain).
    let mpath = vault_manifest_path();
    let out = spawn_and_wait_for_exit(&[
        ("TRITON_ENV", "nonprod"),
        ("TRITON_MANIFEST_PATH", &mpath),
        ("TRITON_VAULT_URL", "http://127.0.0.1:1"),
        ("TRITON_VAULT_TOKEN", "irrelevant"),
        // The SSRF-tempting override.
        ("TRITON_SIGNAL_SIGNALD_ADDR", "tcp://attacker.example:15432"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "non-local env with non-tailnet TRITON_SIGNAL_SIGNALD_ADDR MUST exit 2;\nstderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("NFR-S-4"),
        "exit log MUST mention NFR-S-4; got: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_accepts_tailnet_addr_in_nonprod() {
    // Paired with the rejection test: a `tcp://*.ts.net:port` is on
    // the tailnet allowlist and MUST get past the NFR-S-4 gate. The
    // binary still fails downstream (the manifest's vault refs are
    // unreachable against `http://127.0.0.1:1`) but the FAILURE
    // MODE is what matters: the NFR-S-4 path emits a specific
    // "MUST set TRITON_SIGNAL_SIGNALD_ADDR" + "NFR-S-4" error.
    // A non-allowlist value triggers that error; an allowlist value
    // skips it and the binary fails downstream for a different
    // reason (or boots, depending on Vault availability).
    let mpath = vault_manifest_path();
    let out = spawn_and_wait_for_exit(&[
        ("TRITON_ENV", "nonprod"),
        ("TRITON_MANIFEST_PATH", &mpath),
        ("TRITON_VAULT_URL", "http://127.0.0.1:1"),
        ("TRITON_VAULT_TOKEN", "irrelevant"),
        // Allowlist-passing override.
        (
            "TRITON_SIGNAL_SIGNALD_ADDR",
            "tcp://signald.example.ts.net:15432",
        ),
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Negative assertion: the NFR-S-4 boot-rejection path must NOT
    // have fired. The binary will still exit 2 for the downstream
    // vault-unreachable failure; that's expected. We just need the
    // NFR-S-4 path to have been skipped.
    assert!(
        !combined.contains(
            "non-`local` env MUST set TRITON_SIGNAL_SIGNALD_ADDR \
             to a `unix://...` path or a `tcp://*.ts.net[:port]`"
        ),
        "tailnet addr MUST pass the NFR-S-4 gate; combined output:\n{combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_accepts_unix_socket_addr_in_nonprod() {
    // PR 37 Finding 7 nuance: `unix://...` is a file path, not a
    // network destination, so NFR-S-4 doesn't restrict it. A unix
    // socket override MUST get past the gate regardless of the
    // host suffix (because there is no host).
    let mpath = vault_manifest_path();
    let out = spawn_and_wait_for_exit(&[
        ("TRITON_ENV", "nonprod"),
        ("TRITON_MANIFEST_PATH", &mpath),
        ("TRITON_VAULT_URL", "http://127.0.0.1:1"),
        ("TRITON_VAULT_TOKEN", "irrelevant"),
        (
            "TRITON_SIGNAL_SIGNALD_ADDR",
            "unix:///var/run/signald/signald.sock",
        ),
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains(
            "non-`local` env MUST set TRITON_SIGNAL_SIGNALD_ADDR \
             to a `unix://...` path or a `tcp://*.ts.net[:port]`"
        ),
        "unix:// addr MUST pass the NFR-S-4 gate; combined output:\n{combined}"
    );
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
