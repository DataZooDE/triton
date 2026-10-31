//! Resuming stored Teams tasks after a restart (F8b crew review, items
//! 2–5): exactly one delivery across several adapters, the resume path
//! re-checks what the inbound path checked (serviceUrl, current sender
//! policy, owner), delivery is tracked apart from execution (a crash after
//! completion, an expiry while down), and a resumed failure shows the same
//! sanitized notice as a live one.
//!
//! No mocks: the real binary, `FakeBotFramework`, `FakeAsyncAgent`, a real
//! SQLite task DB.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
use triton_tests::chat_courier_fixture::FakeBotFramework;

const ANSWER: &str = "4 drafts ready for review.";

fn manifest(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
        .display()
        .to_string()
}

fn tmp_db(tag: &str) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "msteams-resume-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("tasks.db")
}

fn env(
    fake: &FakeBotFramework,
    agent: &FakeAsyncAgent,
    manifest_file: &str,
    db: &std::path::Path,
) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest(manifest_file)),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        ("TRITON_MSTEAMS_ASYNC".to_string(), "1".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("answer={}", agent.host_port()),
        ),
        ("TRITON_TASK_DB".to_string(), db.display().to_string()),
    ])
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn post(proc: &TritonProcess, fake: &FakeBotFramework) {
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com", "aud": "triton-msteams-test-appid",
        "exp": now() + 600, "iat": now() - 5, "serviceurl": fake.service_url(),
    }));
    let webhook = proc.chat_webhook_addr.expect("webhook");
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&json!({
            "type": "message", "id": "msg-1",
            "timestamp": "2026-10-31T10:00:00.0000000Z",
            "serviceUrl": "https://placeholder.example/", "channelId": "msteams",
            "from": { "id": "29:1abc", "name": "Alice" },
            "conversation": { "id": "a:conv-1" },
            "recipient": { "id": "28:bot-1", "name": "MyBot" },
            "text": "Propose an owner for every Agent without one.",
            "textFormat": "plain"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
}

async fn wait_until(deadline: Duration, mut probe: impl FnMut() -> bool) {
    let start = Instant::now();
    while !probe() {
        assert!(
            start.elapsed() < deadline,
            "condition not met within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn is_card(body: &Value) -> bool {
    body["attachments"][0]["contentType"] == "application/vnd.microsoft.card.adaptive"
}

fn answered(fake: &FakeBotFramework) -> usize {
    fake.updates()
        .iter()
        .filter(|u| u.body.to_string().contains(ANSWER))
        .count()
        + fake
            .captured()
            .iter()
            .filter(|c| c.body["type"] == "message" && c.body.to_string().contains(ANSWER))
            .count()
}

/// Start, wait for the working card, stop Triton (SIGTERM).
async fn card_then_stop(
    fake: &FakeBotFramework,
    agent: &FakeAsyncAgent,
    manifest_file: &str,
    db: &std::path::Path,
) {
    let mut first =
        TritonProcess::spawn_with_env(Duration::from_secs(10), env(fake, agent, manifest_file, db))
            .await;
    post(&first, fake).await;
    wait_until(Duration::from_secs(10), || {
        fake.captured().iter().any(|c| is_card(&c.body))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.terminate(Duration::from_secs(5));
}

/// Item 2: two msteams adapters (two bots) share the store; after a
/// restart the answer is delivered exactly once, by the adapter that owns
/// the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_adapters_and_a_restart_deliver_exactly_once() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(3),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("two");
    card_then_stop(&fake, &agent, "manifest-msteams-two-adapters.yaml", &db).await;
    let _second = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-two-adapters.yaml", &db),
    )
    .await;
    wait_until(Duration::from_secs(15), || answered(&fake) >= 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        answered(&fake),
        1,
        "exactly one delivery: {:#?}",
        fake.updates()
    );
}

/// Item 3: a serviceUrl the inbound check would refuse is never contacted
/// on resume — a tampered DB cannot send the bot's token elsewhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_service_url_is_never_contacted() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(2),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("tamper");
    card_then_stop(&fake, &agent, "manifest-msteams-async-test.yaml", &db).await;
    let conn = rusqlite::Connection::open(&db).unwrap();
    let n = conn
        .execute(
            "UPDATE triton_tasks SET record = json_set(record, '$.delivery.service_url', \
             'https://attacker.example/')",
            [],
        )
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    let second = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-async-test.yaml", &db),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(answered(&fake), 0);
    assert!(agent.resumes().is_empty(), "the task was not resumed");
    let refused = second
        .stderr_snapshot()
        .iter()
        .chain(second.stdout_snapshot().iter())
        .any(|l| l.contains("resume refused"));
    assert!(refused, "the refusal is logged");
}

/// Item 3: a sender the CURRENT policy no longer admits is not resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_sender_is_not_resumed() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(2),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("revoked");
    card_then_stop(&fake, &agent, "manifest-msteams-async-test.yaml", &db).await;
    let _second = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-async-revoked.yaml", &db),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(answered(&fake), 0, "nothing posted for a revoked sender");
    assert!(agent.resumes().is_empty());
}

/// Item 4: Triton is killed (SIGKILL) while the final card update is in
/// flight — the task is complete but the answer was not confirmed. The
/// restarted Triton delivers the stored answer into the card.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_after_completion_still_delivers_the_answer() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(1200),
        tick: Duration::from_millis(400),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("kill");
    let first = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-async-test.yaml", &db),
    )
    .await;
    post(&first, &fake).await;
    wait_until(Duration::from_secs(10), || {
        fake.captured().iter().any(|c| is_card(&c.body))
    })
    .await;
    fake.set_update_delay(Duration::from_secs(30));
    wait_until(Duration::from_secs(15), || answered(&fake) >= 1).await;
    let pid = first.pid().expect("pid") as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    drop(first);
    fake.set_update_delay(Duration::ZERO);
    let _second = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-async-test.yaml", &db),
    )
    .await;
    wait_until(Duration::from_secs(10), || answered(&fake) >= 2).await;
    assert!(
        agent.resumes().is_empty(),
        "a completed task is delivered, not resumed"
    );
    let last = fake.updates().last().unwrap().clone();
    assert_eq!(last.activity_id, "stub-activity-id", "into the same card");
}

/// Item 4: a task that expired while Triton was down is shown as expired
/// in its card, not left on "working".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_that_expired_while_down_says_so_in_its_card() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(8),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("expired");
    let mut e = env(&fake, &agent, "manifest-msteams-async-test.yaml", &db);
    e.insert("TRITON_TASK_MAX_SECS".into(), "2".into());
    let mut first = TritonProcess::spawn_with_env(Duration::from_secs(10), e.clone()).await;
    post(&first, &fake).await;
    wait_until(Duration::from_secs(10), || {
        fake.captured().iter().any(|c| is_card(&c.body))
    })
    .await;
    first.terminate(Duration::from_secs(5));
    drop(first);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let _second = TritonProcess::spawn_with_env(Duration::from_secs(10), e).await;
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| u.body.to_string().contains("took longer than allowed"))
    })
    .await;
    assert!(agent.resumes().is_empty(), "an expired task is not resumed");
}

/// Item 5: a resumed task that fails shows the same sanitized notice as a
/// live one — never the upstream's raw text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_failure_shows_the_sanitized_notice() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(3),
        fail_with: Some("INTERNAL-SECRET-42 stack trace".into()),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("fail");
    card_then_stop(&fake, &agent, "manifest-msteams-async-test.yaml", &db).await;
    let _second = TritonProcess::spawn_with_env(
        Duration::from_secs(10),
        env(&fake, &agent, "manifest-msteams-async-test.yaml", &db),
    )
    .await;
    wait_until(Duration::from_secs(12), || {
        fake.updates()
            .iter()
            .any(|u| u.body.to_string().contains("I encountered an issue"))
    })
    .await;
    assert!(
        !fake
            .updates()
            .iter()
            .any(|u| u.body.to_string().contains("INTERNAL-SECRET-42")),
        "no raw upstream text reaches the user"
    );
}
