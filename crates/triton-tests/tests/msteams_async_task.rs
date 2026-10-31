//! Teams and a long-running upstream task (mock 1l).
//!
//! When the upstream goes asynchronous (contract §7), the Teams courier
//! stops showing only "typing": it posts ONE card (the receipt as its
//! title, a running timer, the real steps), updates that same activity in place as progress
//! arrives, and finally replaces it with the answer — the Bot Connector's
//! `updateActivity` (`PUT /v3/conversations/{id}/activities/{activityId}`).
//! No second message carries the answer.
//!
//! No mocks: the real binary, the in-repo `FakeBotFramework` (real HTTP,
//! now also capturing `PUT`s) and the SSE-speaking `FakeAsyncAgent`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
use triton_tests::chat_courier_fixture::FakeBotFramework;

const AUDIENCE: &str = "triton-msteams-test-appid";
const ANSWER: &str = "4 drafts ready for review.";

fn env(fake: &FakeBotFramework, agent: &FakeAsyncAgent) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/manifest-msteams-async-test.yaml")
                .display()
                .to_string(),
        ),
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
    ])
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn activity(conversation_type: Option<&str>) -> Value {
    let mut conversation = json!({ "id": "a:conv-1" });
    if let Some(ct) = conversation_type {
        conversation["conversationType"] = json!(ct);
    }
    json!({
        "type": "message", "id": "msg-1",
        "timestamp": "2026-10-31T10:00:00.0000000Z",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": conversation,
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": "Propose an owner for every Agent without one.",
        "textFormat": "plain"
    })
}

async fn post(proc: &TritonProcess, fake: &FakeBotFramework, conversation_type: Option<&str>) {
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com", "aud": AUDIENCE,
        "exp": now() + 600, "iat": now() - 5, "serviceurl": fake.service_url(),
    }));
    let webhook = proc.chat_webhook_addr.expect("webhook");
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity(conversation_type))
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

fn card_text(body: &Value) -> String {
    body.to_string()
}

fn is_card(body: &Value) -> bool {
    body["attachments"][0]["contentType"] == "application/vnd.microsoft.card.adaptive"
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_chat_gets_typing_then_one_card_updated_in_place() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(2),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;

    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;

    let posted = fake.captured();
    assert_eq!(
        posted[0].body["type"], "typing",
        "typing first: {:#?}",
        posted[0].body
    );
    let messages: Vec<&Value> = posted
        .iter()
        .map(|c| &c.body)
        .filter(|b| b["type"] == "message")
        .collect();
    assert_eq!(
        messages.len(),
        1,
        "exactly one message — the card: {messages:#?}"
    );
    assert!(is_card(messages[0]), "{:#}", messages[0]);
    let working = card_text(messages[0]);
    // One anatomy: the receipt is the title, the timer sits beside it; no
    // "REQUEST · WORKING" process label.
    assert!(
        working.contains("Your request is filed and still running."),
        "{working}"
    );
    assert!(working.contains("\"0:0"), "a running timer: {working}");
    assert!(!working.contains("WORKING"), "{working}");
    assert!(
        messages[0]["attachments"][0]["content"]["refresh"]["action"]["type"] == "Action.Execute",
        "the working card refreshes itself: {working}"
    );
    assert!(
        !working.contains(ANSWER),
        "the first card is the working state"
    );

    let updates = fake.updates();
    assert!(
        updates
            .iter()
            .all(|u| u.activity_id == "stub-activity-id" && u.conversation_id == "a:conv-1"),
        "every update addresses the card's own activity: {updates:#?}"
    );
    assert!(
        updates.iter().any(|u| card_text(&u.body).contains("Step ")),
        "progress updates the card in place: {updates:#?}"
    );
    let last = &updates.last().unwrap().body;
    assert!(
        card_text(last).contains(ANSWER),
        "the final update is the answer: {last:#}"
    );
    assert_eq!(last["type"], "message");
    assert_eq!(agent.filed_count(), 1);

    // One audited post line for the delivered answer.
    let start = Instant::now();
    loop {
        let found = proc.stdout_snapshot().iter().any(|l| {
            serde_json::from_str::<Value>(l).is_ok_and(|v| {
                v["kind"] == "audit" && v["phase"] == "post" && v["status_label"] == "posted"
            })
        });
        if found {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "no post audit line"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_personal_chat_closes_the_stream_with_the_card_then_updates_it() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(1500),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, Some("personal")).await;
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
    let posted = fake.captured();
    assert_eq!(posted[0].body["entities"][0]["streamType"], "informative");
    let card = posted
        .iter()
        .map(|c| &c.body)
        .find(|b| b["type"] == "message")
        .expect("the card");
    assert!(is_card(card));
    assert_eq!(
        card["entities"][0]["streamType"], "final",
        "the card closes the informative stream: {card:#}"
    );
    let updates = fake.updates();
    let last = &updates.last().unwrap().body;
    assert!(
        last.get("entities").is_none(),
        "an update carries no stream entities: {last:#}"
    );
}

/// Teams answers a stream's `final` message with `202 {}` and no id; the
/// stream's id is the message's activity id (streaming-ux, "Response
/// codes"). The working card that closes a personal chat's stream must
/// still be updated in place under that id, and the answer must not
/// arrive as a second message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_personal_chat_card_is_updated_under_the_stream_id_when_teams_returns_no_id() {
    let fake = FakeBotFramework::start().await;
    fake.set_teams_streaming(true);
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(1500),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, Some("personal")).await;
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
            || fake
                .captured()
                .iter()
                .any(|c| c.body["type"] == "message" && card_text(&c.body).contains(ANSWER))
    })
    .await;
    let updates = fake.updates();
    assert!(
        !updates.is_empty()
            && updates
                .iter()
                .all(|u| u.activity_id == FakeBotFramework::STREAM_ACTIVITY_ID),
        "the card is updated under the stream's activity id: {updates:#?}"
    );
    let messages: Vec<Value> = fake
        .captured()
        .into_iter()
        .map(|c| c.body)
        .filter(|b| b["type"] == "message")
        .collect();
    assert_eq!(
        messages.len(),
        1,
        "only the working card is posted; the answer replaces it: {messages:#?}"
    );
    drop(proc);
}

/// F8: a refused `updateActivity` must not cost the answer — it is posted
/// as a new message instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_card_update_falls_back_to_posting_the_answer() {
    let fake = FakeBotFramework::start().await;
    fake.set_update_status(500);
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(800),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;
    wait_until(Duration::from_secs(10), || {
        fake.captured()
            .iter()
            .any(|c| c.body["type"] == "message" && card_text(&c.body).contains(ANSWER))
    })
    .await;
}

/// F8: the first step shows at once — the update throttle does not hide
/// progress that arrives in the card's first second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_step_updates_the_card_at_once() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(900),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
    assert!(
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains("Step 1")),
        "the first step updated the card: {:#?}",
        fake.updates()
    );
}

/// A step that arrives inside the update throttle is not lost. The
/// upstream reports three steps within 200 ms and then works quietly for
/// seconds; the card must show the LAST step before the answer arrives,
/// without the upstream pacing its events to the throttle (a trailing
/// flush after the window), and the throttle still holds (no update
/// burst).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_step_shows_even_when_it_arrives_inside_the_throttle() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(4),
        tick: Duration::from_millis(100),
        progress_limit: Some(3),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;
    // Well before the answer (4 s after filing), the card shows step 3.
    wait_until(Duration::from_secs(10), || !fake.updates().is_empty()).await;
    let first_update = Instant::now();
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains("Step 3 of"))
    })
    .await;
    assert!(
        first_update.elapsed() < Duration::from_millis(2500),
        "the trailing flush follows the throttle window, not the answer"
    );
    let updates = fake.updates();
    assert!(
        !updates.iter().any(|u| card_text(&u.body).contains(ANSWER)),
        "step 3 showed before the answer: {updates:#?}"
    );
    assert_eq!(
        updates.len(),
        2,
        "one update for step 1 at once, one trailing flush for step 3: {updates:#?}"
    );
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
}

fn tmp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "msteams-task-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("tasks.db")
}

/// F8b: a Teams turn is a task — an upstream that runs longer than one
/// upstream stream may last (`TRITON_STREAM_MAX_DURATION_MS`) is resumed,
/// not failed, and its card still gets the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teams_task_outlives_the_upstream_stream_cap() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(3),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let mut e = env(&fake, &agent);
    e.insert("TRITON_STREAM_MAX_DURATION_MS".into(), "1000".into());
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), e).await;
    post(&proc, &fake, None).await;
    wait_until(Duration::from_secs(15), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
    assert_eq!(agent.filed_count(), 1, "filed once");
    assert!(!agent.resumes().is_empty(), "the cut stream was resumed");
    assert!(
        !fake
            .updates()
            .iter()
            .any(|u| card_text(&u.body).contains("I encountered an issue")),
        "no failure notice replaced the card"
    );
}

/// F8b: Triton restarts mid-task; the restarted process resumes the
/// stored Teams task and replaces the same card with the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teams_card_gets_its_answer_after_a_triton_restart() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(4),
        tick: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("restart");
    let mut e = env(&fake, &agent);
    e.insert("TRITON_TASK_DB".into(), db.display().to_string());
    let mut first = TritonProcess::spawn_with_env(Duration::from_secs(10), e.clone()).await;
    post(&first, &fake, None).await;
    // The working card is out, then Triton stops.
    wait_until(Duration::from_secs(10), || {
        fake.captured().iter().any(|c| is_card(&c.body))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.terminate(Duration::from_secs(5));
    drop(first);
    assert!(
        !fake
            .updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER)),
        "not answered before the restart"
    );

    let _second = TritonProcess::spawn_with_env(Duration::from_secs(10), e).await;
    wait_until(Duration::from_secs(15), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
    let answered = fake.updates();
    let last = answered.last().unwrap();
    assert_eq!(last.activity_id, "stub-activity-id", "the same card");
    assert_eq!(agent.filed_count(), 1, "the restart did not file it again");
    assert!(agent.resumes().contains(&"up-1".to_string()));
}

fn invoke(data: Value, reply_to: &str) -> Value {
    json!({
        "type": "invoke", "name": "adaptiveCard/action", "id": "inv-1",
        "replyToId": reply_to,
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "value": { "action": { "type": "Action.Execute", "verb": "agentAction", "data": data } },
    })
}

async fn post_invoke(proc: &TritonProcess, fake: &FakeBotFramework, body: &Value) -> Value {
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com", "aud": AUDIENCE,
        "exp": now() + 600, "iat": now() - 5, "serviceurl": fake.service_url(),
    }));
    let webhook = proc.chat_webhook_addr.expect("webhook");
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    resp.json().await.unwrap()
}

fn find_execute<'a>(v: &'a Value, title: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if m.get("type").and_then(Value::as_str) == Some("Action.Execute")
                && m.get("title").and_then(Value::as_str) == Some(title)
            {
                return Some(v);
            }
            m.values().find_map(|x| find_execute(x, title))
        }
        Value::Array(a) => a.iter().find_map(|x| find_execute(x, title)),
        _ => None,
    }
}

/// One card per request (turn 3, rule 2): a click on a card's button turns
/// THAT card into the working card (the invoke's card response, no "Working
/// on it" banner), and the answer replaces the same card in place — no
/// second card, no typing. The working card's `refresh` returns the answer
/// once the task is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_click_is_answered_in_the_clicked_card() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(2),
        tick: Duration::from_millis(300),
        result: json!({ "surface": { "components": [
            { "kind": "text", "value": ANSWER },
            { "kind": "button", "label": "Again",
              "action": { "type": "send_message", "text": "Propose an owner again." } }
        ] } }),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;
    wait_until(Duration::from_secs(10), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains("Again"))
    })
    .await;
    let answer = fake.updates().last().unwrap().body.clone();
    let again = find_execute(&answer, "Again").expect("chip").clone();
    let messages_before = fake.captured().len();
    let updates_before = fake.updates().len();

    // The click: the invoke answers with a card (in place), not a message.
    let resp = post_invoke(&proc, &fake, &invoke(again["data"].clone(), "card-7")).await;
    assert_eq!(
        resp["type"], "application/vnd.microsoft.card.adaptive",
        "{resp}"
    );
    assert!(
        !resp.to_string().contains("Working on it \u{2014}"),
        "no banner: {resp}"
    );
    let working = resp["value"].clone();
    assert!(working.to_string().contains("\"0:0"), "a timer: {working}");

    // The answer replaces the clicked card; nothing new is posted.
    wait_until(Duration::from_secs(10), || {
        fake.updates()[updates_before..]
            .iter()
            .any(|u| u.activity_id == "card-7" && card_text(&u.body).contains(ANSWER))
    })
    .await;
    assert_eq!(agent.filed_count(), 2);
    assert!(
        fake.updates()[updates_before..]
            .iter()
            .all(|u| u.activity_id == "card-7"),
        "every update after the click addresses the clicked card"
    );
    assert_eq!(
        fake.captured().len(),
        messages_before,
        "no second card, no typing: {:#?}",
        &fake.captured()[messages_before..]
    );

    // The working card's refresh now returns the answer card.
    let updated = fake.updates()[updates_before..]
        .iter()
        .find(|u| u.body["attachments"][0]["content"]["refresh"].is_object())
        .map(|u| u.body["attachments"][0]["content"]["refresh"]["action"]["data"].clone())
        .expect("a working-card update carries the refresh action");
    let refreshed = post_invoke(&proc, &fake, &invoke(updated, "card-7")).await;
    assert_eq!(
        refreshed["type"], "application/vnd.microsoft.card.adaptive",
        "{refreshed}"
    );
    assert!(refreshed.to_string().contains(ANSWER), "{refreshed}");
}

/// The timer never sticks at 0:00 (3f): with no new step, the working card
/// is still pushed while the run lasts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_working_card_timer_runs_without_new_steps() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(13),
        tick: Duration::from_millis(200),
        progress_limit: Some(1),
        ..AsyncPlan::default()
    })
    .await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(10), env(&fake, &agent)).await;
    post(&proc, &fake, None).await;
    wait_until(Duration::from_secs(20), || {
        fake.updates()
            .iter()
            .any(|u| card_text(&u.body).contains(ANSWER))
    })
    .await;
    let ticks: Vec<String> = fake
        .updates()
        .iter()
        .filter(|u| !card_text(&u.body).contains(ANSWER))
        .map(|u| card_text(&u.body))
        .collect();
    assert!(
        ticks.iter().any(|t| t.contains("\"0:1")),
        "a tick at 0:1x with no new step: {ticks:#?}"
    );
}
