//! Guards the shipped demo deployment manifest
//! (`deploy/gateway/adapter.yaml`): it must validate under
//! `TRITON_ENV=nonprod` (PRODUCTION manifest mode — literal
//! credentials rejected) and its `vault://` references must resolve
//! against a real Vault before the gateway boots.
//!
//! No mocks per CLAUDE.md §1: spawns the real `triton` binary with a
//! real `FakeVault` (KV v2 wire shape) serving the demo's Telegram
//! credentials, then asserts the process reaches `/healthz` — which
//! it only does after the manifest closed-checks pass, every Vault
//! ref resolves, and the Telegram long-poll worker is wired.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";

/// Absolute path to the shipped demo manifest (repo `deploy/gateway/`).
fn deploy_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/gateway/adapter.yaml")
        .canonicalize()
        .expect("deploy/gateway/adapter.yaml exists")
        .display()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_demo_manifest_validates_and_boots_under_nonprod() {
    // Serve the exact Vault path the manifest references. The KV v2
    // fake requires the `kv/data/...` segment verbatim (the manifest's
    // `vault://<path>#<field>` includes it).
    let vault = FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/apps/dz/triton/nonprod/telegram",
            &[
                ("webhook_secret", "unused-in-long-poll"),
                ("bot_token", "12345:demo-bot-token"),
                (
                    "senders",
                    r#"{"42":{"sub":"demo","scopes":["chat"],"tenant":"demo"}}"#,
                ),
                ("correlation_key", "correlation-key-32-bytes-or-more!!"),
            ],
        )],
    )
    .await;

    let env = HashMap::from([
        // PRODUCTION manifest validation (rejects literal creds).
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), deploy_manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
        // Canonical Telegram base is mandatory outside `local` env
        // (NFR-S-4); the long-poll worker will poll it and back off if
        // unreachable from the test host — boot is unaffected.
        (
            "TRITON_TELEGRAM_API_BASE".to_string(),
            "https://api.telegram.org".to_string(),
        ),
    ]);

    // `spawn_with_env` panics if the process exits before `/healthz`,
    // i.e. if the manifest fails to validate or a Vault ref can't
    // resolve. Reaching ready IS the assertion.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // Belt-and-braces: confirm the manifest loaded and the long-poll
    // worker started (the demo's Telegram inbound shape).
    let saw_manifest = wait_for_log(&proc, Duration::from_secs(3), |l| {
        l.contains("adapter.yaml loaded")
    });
    assert!(saw_manifest, "manifest should load under nonprod");
    let saw_worker = wait_for_log(&proc, Duration::from_secs(3), |l| {
        l.contains("telegram long-poll worker started")
    });
    assert!(
        saw_worker,
        "long-poll worker should start for the demo telegram adapter"
    );
}

fn wait_for_log(proc: &TritonProcess, deadline: Duration, pred: impl Fn(&str) -> bool) -> bool {
    let start = std::time::Instant::now();
    loop {
        if proc.stdout_snapshot().iter().any(|l| pred(l))
            || proc.stderr_snapshot().iter().any(|l| pred(l))
        {
            return true;
        }
        if start.elapsed() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
