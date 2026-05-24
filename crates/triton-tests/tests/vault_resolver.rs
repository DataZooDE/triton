//! v0.2 PR 16 — Vault KV v2 secret resolver.
//!
//! Proves the resolver lifts PR 13's warn-and-skip carve-out: a
//! manifest whose every adapter credential is a `vault://` ref now
//! boots the Telegram webhook end-to-end, and the resolved
//! `secret_token` actually authenticates inbound updates.
//!
//! No mocks: real binary, real HTTP, real KV v2 wire shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeVault;

const VAULT_TOKEN: &str = "triton-vault-token";
const RESOLVED_SECRET: &str = "secret-resolved-from-vault";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

fn telegram_update(user_id: u64, text: &str) -> Value {
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "Alice" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

async fn start_kv_vault() -> FakeVault {
    FakeVault::start_kv_v2(
        VAULT_TOKEN,
        &[(
            "kv/data/triton-test/telegram",
            &[
                ("webhook_secret", RESOLVED_SECRET),
                ("bot_token", "bot-token-from-vault"),
                (
                    "senders",
                    r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#,
                ),
                ("correlation_key", "correlation-key-from-vault"),
            ],
        )],
    )
    .await
}

fn env_with_vault(vault: &FakeVault) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_TOKEN".to_string(), VAULT_TOKEN.to_string()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_authenticates_with_vault_resolved_secret() {
    let vault = start_kv_vault().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_vault(&vault)).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_update(42, "hello via vault"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "expected 200, got {}",
        resp.status()
    );

    let audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:telegram"
    });
    assert_eq!(audit["tool"], "echo");
    assert_eq!(audit["who"], "alice");
    assert_eq!(audit["tenant"], "acme");
    assert_eq!(audit["result"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_refuses_boot_when_vault_unreachable() {
    // The manifest declares vault:// refs but TRITON_VAULT_URL
    // points at a port nothing listens on. PR 13's behaviour was
    // warn-and-skip; PR 16 promotes this to a hard refusal so a
    // misconfigured prod deploy can't serve traffic with a silently
    // missing adapter.
    let bin = locate_triton_binary();
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path())
        // Port 1 is reserved; nothing listens there. Resolver will
        // fail-closed and the binary should exit non-zero.
        .env("TRITON_VAULT_URL", "http://127.0.0.1:1")
        .env("TRITON_VAULT_TOKEN", VAULT_TOKEN)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "binary should exit 2 when the resolver can't reach Vault; got {:?}",
        out.status.code()
    );
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
