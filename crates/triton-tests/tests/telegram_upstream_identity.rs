//! `identity.kind: upstream` for the Telegram adapter (FR-I-7).
//!
//! Port of the WhatsApp adapter's upstream identity strategy: instead
//! of an operator-enumerated `sender_table`, the adapter delegates
//! sender resolution to a resolver tool reached through the upstream
//! router. The resolver receives `{platform: "telegram", sender:
//! <user_id>}` and returns `{sub, scopes, tenant}`; any failure
//! rejects the inbound with 401 — never a guessed principal. This is
//! what enables self-onboarding: a brand-new Telegram sender (absent
//! from any static table) is resolved dynamically.
//!
//! Both the resolver and the command agent are REAL HTTP endpoints
//! (`FakeAgent`s) reached via `TRITON_STATIC_UPSTREAMS` — no mocks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::upstream_fixture::FakeAgent;

const SECRET: &str = "webhook-secret-for-test";
/// Not present in any sender table — only the resolver knows it.
const UNKNOWN_USER_ID: u64 = 42;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-upstream-identity.yaml")
        .display()
        .to_string()
}

fn telegram_update(user_id: u64, text: &str) -> Value {
    json!({
        "update_id": 100,
        "message": {
            "message_id": 1,
            "from": { "id": user_id, "is_bot": false, "first_name": "Mallory" },
            "chat": { "id": user_id, "type": "private" },
            "date": 1_700_000_000,
            "text": text
        }
    })
}

fn env_for(
    telegram: &FakeTelegramApi,
    agent: &FakeAgent,
    resolver: &FakeAgent,
) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TELEGRAM_API_BASE".to_string(), telegram.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "assistant={},resolve_identity={}",
                agent.host_port(),
                resolver.host_port()
            ),
        ),
    ])
}

async fn post_inbound(proc: &TritonProcess, user_id: u64, text: &str) -> reqwest::Response {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    reqwest::Client::new()
        .post(format!("http://{webhook}/telegram/webhook"))
        .header("X-Telegram-Bot-Api-Secret-Token", SECRET)
        .json(&telegram_update(user_id, text))
        .send()
        .await
        .expect("POST inbound webhook")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_identity_resolves_principal_and_dispatches_with_it() {
    let resolver = FakeAgent::start_returning(json!({
        "sub": "telegram:42",
        "scopes": ["chat"],
        "tenant": "globex"
    }))
    .await;
    let agent = FakeAgent::start_echoing().await;
    let telegram = FakeTelegramApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&telegram, &agent, &resolver),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_USER_ID, "hello via resolver").await;
    assert!(resp.status().is_success(), "{}", resp.status());

    // The resolver was called with the platform + sender pair.
    let resolver_bodies = wait_for(Duration::from_secs(3), || {
        let v = resolver.bodies_seen();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(
        resolver_bodies[0],
        json!({ "platform": "telegram", "sender": UNKNOWN_USER_ID.to_string() }),
        "resolver must receive {{platform, sender}}"
    );
    assert_eq!(resolver.hits(), 1, "resolver agent must be called once");

    // The resolve call itself is audited under the dedicated
    // identity protocol label, distinct from the command's.
    let resolve_audit = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram:identity"
    });
    assert_eq!(resolve_audit["tool"], "resolve_identity");
    assert_eq!(resolve_audit["result"], "ok");

    // The REAL command dispatch carries the principal the resolver
    // returned — sub and tenant come from the resolver's reply.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "dispatch"
            && v["protocol"] == "messenger:telegram"
            && v["tool"] == "assistant"
    });
    assert_eq!(dispatch["who"], "telegram:42", "sub from resolver tool");
    assert_eq!(dispatch["tenant"], "globex", "tenant from resolver tool");
    assert_eq!(dispatch["result"], "ok");

    // The command agent received the plain-text args ...
    let agent_bodies = wait_for(Duration::from_secs(3), || {
        let v = agent.bodies_seen();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(agent_bodies[0], json!({ "message": "hello via resolver" }));

    // ... and the reply was couriered back to the platform.
    let sent = wait_for(Duration::from_secs(3), || {
        let v = telegram.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(sent[0].body["chat_id"], UNKNOWN_USER_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_identity_rejects_with_401_when_resolver_fails() {
    let resolver = FakeAgent::start_always_failing().await;
    let agent = FakeAgent::start_echoing().await;
    let telegram = FakeTelegramApi::start().await;

    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&telegram, &agent, &resolver),
    )
    .await;

    let resp = post_inbound(&proc, UNKNOWN_USER_ID, "resolver will fail").await;
    assert_eq!(
        resp.status(),
        401,
        "resolver failure must reject the inbound"
    );

    // No command dispatch may occur, and nothing reaches the platform.
    std::thread::sleep(Duration::from_millis(300));
    let assistant_dispatches = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:telegram"
                && v["tool"] == "assistant"
        })
        .count();
    assert_eq!(
        assistant_dispatches, 0,
        "no command dispatch on resolver failure"
    );
    assert_eq!(agent.hits(), 0, "command agent must never be reached");
    assert!(
        telegram.captured().is_empty(),
        "nothing must be couriered on a rejected inbound"
    );

    // The rejection is audited.
    let rejection = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:telegram"
            && v["result"] == "error:auth"
    });
    let _ = rejection;
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe timed out after {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
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
