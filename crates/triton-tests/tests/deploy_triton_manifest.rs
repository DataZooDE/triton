//! Guards the shipped dz-triton chat manifest
//! (`deploy/triton/adapter.yaml`, baked into the image at
//! `/etc/triton/adapter.yaml`): it must validate under `TRITON_ENV=nonprod`
//! (PRODUCTION manifest mode — literal credentials rejected) and every
//! `env://` reference must resolve from the container environment before
//! the gateway boots. This is how the Vault-less substrate delivers the
//! WhatsApp Cloud credentials (GCP Secret Manager → kamal → env, #120).
//!
//! No mocks per CLAUDE.md §1: spawns the real `triton` binary with the
//! exact shipped manifest + the env the substrate would inject, then
//! asserts `/healthz` — which it only reaches after the manifest
//! closed-checks pass, every credential resolves, and the WhatsApp Cloud
//! webhook adapter is wired.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use triton_tests::TritonProcess;

/// Absolute path to the shipped dz-triton manifest (repo `deploy/triton/`).
fn deploy_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/triton/adapter.yaml")
        .canonicalize()
        .expect("deploy/triton/adapter.yaml exists")
        .display()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_dz_triton_manifest_validates_and_boots_under_nonprod() {
    let env = HashMap::from([
        // PRODUCTION manifest validation (rejects literal creds; env:// ok).
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), deploy_manifest_path()),
        // Canonical WhatsApp base is mandatory outside `local` (egress
        // allowlist); the rasterizer host must be non-loopback.
        (
            "TRITON_WHATSAPP_API_BASE".to_string(),
            "https://graph.facebook.com".to_string(),
        ),
        (
            "TRITON_RASTERIZER_URL".to_string(),
            "https://rasterizer.nonprod.int.data-zoo.de".to_string(),
        ),
        // The env:// credential vars the substrate injects from GCP SM.
        (
            "TRITON_WHATSAPP_APP_SECRET".to_string(),
            "app-secret-stand-in".to_string(),
        ),
        (
            "TRITON_WHATSAPP_VERIFY_TOKEN".to_string(),
            "verify-token-stand-in".to_string(),
        ),
        (
            "TRITON_WHATSAPP_ACCESS_TOKEN".to_string(),
            "access-token-stand-in".to_string(),
        ),
        (
            "TRITON_WHATSAPP_PHONE_NUMBER_ID".to_string(),
            "100200300".to_string(),
        ),
        (
            "TRITON_WHATSAPP_SENDER_TABLE".to_string(),
            r#"{"491700000000":{"sub":"operator","scopes":["chat"],"tenant":"default"}}"#
                .to_string(),
        ),
        (
            "TRITON_WHATSAPP_CORRELATION_KEY".to_string(),
            "correlation-key-32-bytes-or-more!!".to_string(),
        ),
    ]);

    // Reaches /healthz only after manifest closed-checks pass, all env://
    // refs resolve, and the WhatsApp webhook adapter wires. spawn_with_env
    // panics on early exit (a rejected manifest / unset env var).
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert_eq!(
        resp.status(),
        200,
        "dz-triton boots with the shipped env:// WhatsApp manifest under nonprod"
    );
}
