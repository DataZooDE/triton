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
async fn signal_rejects_non_loopback_addr_in_nonprod() {
    // PR 37 Finding 7 (MEDIUM, NFR-S-4): the previous check only
    // refused an EMPTY TRITON_SIGNAL_SIGNALD_ADDR outside `local`.
    // Setting it to anything non-empty was accepted, so an operator
    // (or a compromised env var) could redirect signald connections
    // at an arbitrary host. The fix: outside `local`, the tcp:// host
    // MUST be loopback (#288 tightened this from the since-retired
    // `.ts.net` tailnet suffix to what FR-I-9 always said).
    let mpath = vault_manifest_path();
    let out = spawn_and_wait_for_exit(&[
        ("TRITON_ENV", "nonprod"),
        ("TRITON_MANIFEST_PATH", &mpath),
        // The SSRF-tempting override.
        ("TRITON_SIGNAL_SIGNALD_ADDR", "tcp://attacker.example:15432"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "non-local env with non-loopback TRITON_SIGNAL_SIGNALD_ADDR MUST exit 2;\nstderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("NFR-S-4") && combined.contains("FR-I-9"),
        "exit log MUST name both rules it enforces; got: {combined}"
    );
}

/// #288: the gate checked for a `.ts.net` suffix — a network that has
/// been decommissioned — while FR-I-9 and NFR-S-6 say LOOPBACK, and the
/// traceability table claimed PASS. A table claiming PASS for a rule the
/// code does not implement is worse than either the rule or its absence,
/// because it stops anyone looking.
///
/// The premise that justified the looser check is gone, so the code
/// tightens to what the spec always said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_rejects_a_tailnet_addr_now_that_the_tailnet_is_gone() {
    let mpath = vault_manifest_path();
    let out = spawn_and_wait_for_exit(&[
        ("TRITON_ENV", "nonprod"),
        ("TRITON_MANIFEST_PATH", &mpath),
        // Valid under the OLD rule; the tailnet no longer exists.
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
    // Both the refusal and the accept path exit 2 here (the fixture's
    // `env://` credentials are unset, so boot fails downstream either
    // way). The exit code is therefore NOT discriminating — the rule
    // named in the refusal line is.
    assert!(
        combined.contains("FR-I-9"),
        "a `.ts.net` target MUST no longer pass — the network is gone \
         and FR-I-9 requires loopback; got:\n{combined}"
    );
    assert_eq!(out.status.code(), Some(2), "refusal exits 2");
}

/// FR-I-9 spells out what IS allowed: IPv4 loopback, IPv6 loopback, or a
/// unix socket. Each must get past the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_accepts_every_loopback_form_fr_i_9_names() {
    let mpath = vault_manifest_path();
    for addr in [
        "tcp://127.0.0.1:15432",
        "tcp://127.0.0.5:15432",
        "tcp://[::1]:15432",
        "unix:///var/run/signald/signald.sock",
    ] {
        let out = spawn_and_wait_for_exit(&[
            ("TRITON_ENV", "nonprod"),
            ("TRITON_MANIFEST_PATH", &mpath),
            ("TRITON_SIGNAL_SIGNALD_ADDR", addr),
        ]);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // Asserting the ABSENCE of a refusal would pass vacuously while
        // the gate refuses these for a different reason, so assert the
        // POSITIVE evidence that boot reached past the gate: the adapter
        // build runs and fails downstream on the fixture's unset
        // `env://` credentials.
        assert!(
            combined.contains("signal adapter build failed"),
            "`{addr}` is a form FR-I-9 explicitly permits and must reach \
             adapter construction; got: {combined}"
        );
        assert!(
            !combined.contains("FR-I-9"),
            "`{addr}` must not trigger the locality refusal; got: {combined}"
        );
    }
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
        combined.contains("signal adapter build failed") && !combined.contains("FR-I-9"),
        "unix:// addr MUST pass the locality gate; combined output:\n{combined}"
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
    let initial_connections = signald.connections();
    let initial_subscribes = signald.count_with_type("subscribe");
    assert!(initial_connections >= 1);

    // Force-close. Adapter should reconnect within ~5s
    // (initial backoff 500ms; we give some slack for accept
    // race + reconnect handshake).
    signald.force_disconnect();

    // Wait for an additional subscribe — implies the adapter
    // reconnected and re-issued it.
    let start = Instant::now();
    let deadline = Duration::from_secs(8);
    loop {
        if signald.count_with_type("subscribe") > initial_subscribes {
            break;
        }
        if start.elapsed() > deadline {
            panic!(
                "expected at least {} subscribe lines after reconnect; got {} (connections={})",
                initial_subscribes + 1,
                signald.count_with_type("subscribe"),
                signald.connections()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(signald.connections() > initial_connections);
}
