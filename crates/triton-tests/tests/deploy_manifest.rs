//! Guards the shipped gateway deployment manifest
//! (`deploy/triton/adapter.yaml`): it must validate under
//! `TRITON_ENV=nonprod` (PRODUCTION manifest mode — literal
//! credentials rejected) and every `env://` reference must resolve
//! from the process environment before the gateway boots.
//!
//! No mocks per CLAUDE.md §1: spawns the real `triton` binary with the
//! adapters' credentials injected as env vars (the Vault-less substrate
//! path — GCP Secret Manager → kamal `.kamal/secrets` → container env),
//! then asserts the process reaches `/healthz` — which it only does
//! after the manifest closed-checks pass, every `env://` ref resolves,
//! and both chat adapters (WhatsApp Cloud + Telegram webhooks) wire.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use triton_tests::TritonProcess;

/// Absolute path to the shipped gateway manifest (repo `deploy/triton/`).
fn deploy_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/triton/adapter.yaml")
        .canonicalize()
        .expect("deploy/triton/adapter.yaml exists")
        .display()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_gateway_manifest_validates_and_boots_under_nonprod() {
    // Inject every credential the manifest references as an `env://`
    // ref. Under nonprod the manifest validator rejects literals, so
    // reaching `/healthz` proves all refs resolved and both adapters
    // wired. The api bases default to their canonical hosts
    // (api.telegram.org / graph.facebook.com), satisfying the NFR-S-4
    // egress allowlist without extra env. The rasterizer URL must be a
    // tailnet-shaped host outside `local` (both adapters declare
    // `dashboard: rasterised_png`); it is never contacted at boot.
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), deploy_manifest_path()),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            "http://rasterizer.tailnet.ts.net:9320".to_string(),
        ),
        // WhatsApp Cloud credentials.
        (
            "TRITON_WHATSAPP_APP_SECRET".to_string(),
            "wa-app-secret".to_string(),
        ),
        (
            "TRITON_WHATSAPP_VERIFY_TOKEN".to_string(),
            "wa-verify-token".to_string(),
        ),
        (
            "TRITON_WHATSAPP_ACCESS_TOKEN".to_string(),
            "wa-access-token".to_string(),
        ),
        (
            "TRITON_WHATSAPP_PHONE_NUMBER_ID".to_string(),
            "1234567890".to_string(),
        ),
        (
            "TRITON_WHATSAPP_SENDER_TABLE".to_string(),
            r#"{"15551234567":{"sub":"demo","scopes":["chat"],"tenant":"default"}}"#.to_string(),
        ),
        (
            "TRITON_WHATSAPP_CORRELATION_KEY".to_string(),
            "correlation-key-32-bytes-or-more!!".to_string(),
        ),
        // Telegram credentials (identity.kind=upstream → no sender table).
        (
            "TRITON_TELEGRAM_SECRET_TOKEN".to_string(),
            "tg-secret-token".to_string(),
        ),
        (
            "TRITON_TELEGRAM_BOT_TOKEN".to_string(),
            "12345:tg-bot-token".to_string(),
        ),
        (
            "TRITON_TELEGRAM_CORRELATION_KEY".to_string(),
            "correlation-key-32-bytes-or-more!!".to_string(),
        ),
    ]);

    // `spawn_with_env` panics if the process exits before `/healthz`,
    // i.e. if the manifest fails to validate or an `env://` ref can't
    // resolve. Reaching ready IS the assertion.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    // Belt-and-braces: confirm the manifest loaded.
    let saw_manifest = wait_for_log(&proc, Duration::from_secs(3), |l| {
        l.contains("adapter.yaml loaded")
    });
    assert!(saw_manifest, "manifest should load under nonprod");
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
