//! #287 — an operator lever that revokes a principal FASTER than its
//! token expires.
//!
//! Every secret in Triton is boot-time-only and every token runs to its
//! own expiry. Between the moment an operator learns a principal is
//! compromised and the moment its token lapses, there was nothing to
//! pull: rotating the signing key invalidates everyone, and there is no
//! shorter lever. For an OIDC access token that window is however long
//! the issuer chose — typically an hour, sometimes a day.
//!
//! `TRITON_DENIED_PRINCIPALS` is that lever. It is checked at the
//! DISPATCHER, which is the single audit pivot (ADR-6) and the one
//! place every protocol converges: MCP, A2A, REST and all eight chat
//! adapters. Putting it at any one boundary would leave the others
//! open.
//!
//! What it is not: an authorization system. It is a kill switch, and
//! the tests below pin the two properties a kill switch needs — it
//! refuses the named principal on every protocol, and it leaves
//! everyone else alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;

const RESOLVED_SECRET: &str = "secret-resolved-from-vault";
const BOT_TOKEN: &str = "12345:resolved-bot-token";
const CORRELATION_KEY: &str = "32byte-correlation-key-for-test!";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

fn env_with(telegram: &FakeTelegramApi, denylist: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        (
            "TRITON_TG_WEBHOOK_SECRET".to_string(),
            RESOLVED_SECRET.to_string(),
        ),
        ("TRITON_TG_BOT_TOKEN".to_string(), BOT_TOKEN.to_string()),
        (
            "TRITON_TG_SENDERS".to_string(),
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
                "43":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#
                .to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            CORRELATION_KEY.to_string(),
        ),
        ("TRITON_DENIED_PRINCIPALS".to_string(), denylist.to_string()),
    ])
}

fn telegram_message(user_id: u64, text: &str) -> Value {
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "User" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

async fn send(webhook_addr: std::net::SocketAddr, user_id: u64) -> u16 {
    reqwest::Client::new()
        .post(format!("http://{webhook_addr}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", RESOLVED_SECRET)
        .json(&telegram_message(user_id, "/narrate alice"))
        .send()
        .await
        .expect("POST inbound")
        .status()
        .as_u16()
}

/// The lever works: the named principal is refused and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denylisted_principal_is_refused_at_the_dispatcher() {
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, "acme/alice"))
            .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    send(webhook_addr, 42).await;

    let rejected = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["who"] == "alice"
    });
    assert_eq!(
        rejected["result"], "error:forbidden",
        "a revoked principal is AUTHENTICATED and then refused — the audit \
         class has to say so, or an operator cannot tell a revocation from \
         a bad credential; got: {rejected}",
    );
    // The tool must not have RUN. The reply is the observable proof:
    // narrate always answers, so silence means it never executed.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        telegram.captured().is_empty(),
        "a denied principal must produce no reply: {:?}",
        telegram.captured().len(),
    );
}

/// …and only the named principal. A kill switch that takes out the
/// tenant is not a kill switch, it is an outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denylist_entry_does_not_touch_anyone_else() {
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, "acme/alice"))
            .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    // bob shares alice's tenant and differs only in `sub`.
    send(webhook_addr, 43).await;

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["who"] == "bob"
    });
    assert_eq!(dispatch["result"], "ok", "bob must be unaffected");
}

/// The entry is tenant-qualified. `alice` in one tenant and `alice` in
/// another are different people, and a bare `sub` would deny both —
/// silently, and across a customer boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_for_another_tenants_alice_does_not_deny_this_one() {
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, "globex/alice"))
            .await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    send(webhook_addr, 42).await;

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["who"] == "alice"
    });
    assert_eq!(
        dispatch["result"], "ok",
        "`globex/alice` must not deny `acme/alice`",
    );
}

/// The dispatcher is the pivot precisely so this holds on EVERY
/// protocol, not just the one it was tested on. The REST path
/// authenticates through a completely different boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_denylist_covers_the_http_trio_too() {
    let telegram = FakeTelegramApi::start().await;
    // The dev-token principal is `dev/dev-user`.
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, "dev/dev-user"))
            .await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/narrate"))
        .bearer_auth("dev-token")
        .json(&json!({ "subject": "alice" }))
        .send()
        .await
        .expect("POST /v1/tools/narrate");

    assert_eq!(
        resp.status(),
        403,
        "a denied principal is authenticated but not permitted — 403, not 401",
    );
    let rejected = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["who"] == "dev-user" && v["result"] == "error:forbidden"
    });
    assert_eq!(rejected["protocol"], "rest");
}

/// An empty or absent list denies nobody. The default must be inert:
/// an operator who never sets this variable must not discover it by
/// having their gateway refuse everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_denylist_denies_nobody() {
    let telegram = FakeTelegramApi::start().await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&telegram, " , ")).await;
    let webhook_addr = proc.chat_webhook_addr.expect("chat webhook listener bound");

    send(webhook_addr, 42).await;

    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["who"] == "alice"
    });
    assert_eq!(dispatch["result"], "ok");
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
