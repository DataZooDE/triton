//! `TRITON_A2UI_ACTION_KEY` is validated at boot.
//!
//! The key signs the Gemini Enterprise card actions; every replica must hold
//! the same one or a click that lands elsewhere silently does nothing. A
//! malformed key used to be swapped for a random per-process key without a
//! word. Now: set-but-invalid aborts boot; unset boots with a warning that
//! clicks resolve on this replica only; valid boots quietly.

use std::process::{Command, Stdio};
use std::time::Duration;

use triton_tests::TritonProcess;

fn boot_once(key: &str) -> std::process::Output {
    let mut child = Command::new(triton_tests::locate_triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_A2UI_ACTION_KEY", key)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn triton");
    // A binary that (wrongly) boots must not hang the test: give it a few
    // seconds to exit, then kill it — a still-running process is exactly
    // the regression this guards.
    let start = std::time::Instant::now();
    while child.try_wait().expect("wait").is_none() {
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("boot with TRITON_A2UI_ACTION_KEY={key:?} did not abort");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().expect("output")
}

#[test]
fn a_malformed_action_key_fails_boot_closed() {
    // Under the 16-byte floor: brute-forceable from one observed card id.
    let out = boot_once("short");
    assert!(!out.status.success(), "a malformed key must abort boot");
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        logs.contains("TRITON_A2UI_ACTION_KEY"),
        "the error names the variable: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unset_action_key_boots_with_a_single_replica_warning() {
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), Default::default()).await;
    std::thread::sleep(Duration::from_millis(200));
    let logs = triton.stdout_snapshot().join("\n") + &triton.stderr_snapshot().join("\n");
    assert!(
        logs.contains("TRITON_A2UI_ACTION_KEY") && logs.contains("single replica"),
        "unset key must be warned about: {logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_action_key_boots_without_the_warning() {
    let env = [(
        "TRITON_A2UI_ACTION_KEY".to_string(),
        "a-shared-action-key-of-32-bytes!".to_string(),
    )]
    .into_iter()
    .collect();
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    std::thread::sleep(Duration::from_millis(200));
    let logs = triton.stdout_snapshot().join("\n") + &triton.stderr_snapshot().join("\n");
    assert!(!logs.contains("single replica"), "{logs}");
}
