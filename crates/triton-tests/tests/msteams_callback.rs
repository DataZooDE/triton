//! Issue #155 — Microsoft Teams interactive Adaptive Card round-trip.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256 JWT
//! verification against the in-repo `FakeBotFramework` fixture.
//!
//! Drives the full inbound → outbound → callback cycle. `/narrate
//! alice` runs the narrate tool, which emits a Surface (Text +
//! Narration + Button); the adapter renders it as an Adaptive Card
//! attachment whose `Action.Execute` carries an HMAC-signed correlation
//! token in `data.ct`. We intercept the reply Activity the courier
//! POSTed and pull the token out of
//! `attachments[0].content.actions[0].data.ct`, then exercise both
//! callback channels:
//!
//! - `Action.Execute` — POST an `invoke` Activity carrying the token.
//!   Triton verifies the HMAC, re-dispatches narrate(alice), and
//!   returns a REFRESHED Adaptive Card in the HTTP response body.
//! - `Action.Submit` — POST a `message`-with-`value` Activity carrying
//!   the token. Triton verifies, re-dispatches, and POSTs a reply
//!   Activity back through the bot connector.
//!
//! Plus negative cases: a token signed under a different key is
//! rejected at the inbound boundary with a `phase: rejected` /
//! `result: error:auth` audit and never re-dispatches.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-test.yaml")
        .display()
        .to_string()
}

fn env_with(fake: &FakeBotFramework) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn good_claims(fake: &FakeBotFramework) -> Value {
    json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceUrl": fake.service_url(),
    })
}

fn message_activity(text: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": text,
        "textFormat": "plain"
    })
}

/// An `invoke` Activity — the Action.Execute (universal action)
/// callback Teams posts when a card button with a `verb` is tapped.
fn invoke_activity(token: &str) -> Value {
    json!({
        "type": "invoke",
        "name": "adaptiveCard/action",
        "id": "inv-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "value": {
            "action": {
                "type": "Action.Execute",
                "verb": "agentAction",
                "data": { "ct": token }
            }
        }
    })
}

/// A `message`-with-`value` Activity — the Action.Submit callback Teams
/// posts on hosts that downgrade a universal action to a classic submit.
fn submit_activity(token: &str) -> Value {
    json!({
        "type": "message",
        "id": "sub-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "value": { "ct": token }
    })
}

/// Pull the signed correlation token out of the first Adaptive Card
/// attachment's top-level `Action.Execute`.
fn token_from_reply(body: &Value) -> String {
    let content = &body["attachments"][0]["content"];
    assert_eq!(
        content["type"], "AdaptiveCard",
        "reply attachment must be an AdaptiveCard; got: {body}"
    );
    content["actions"][0]["data"]["ct"]
        .as_str()
        .unwrap_or_else(|| panic!("no Action.Execute data.ct in card; got: {body}"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrate_button_renders_adaptive_card_and_execute_refreshes_in_place() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let client = reqwest::Client::new();

    // Step 1: /narrate alice → narrate emits Text + Narration + Button.
    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = client
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("/narrate alice"))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Step 2: the reply Activity carries an Adaptive Card whose action
    // holds the signed token.
    let first = wait_for(Duration::from_secs(3), || {
        fake.captured().into_iter().next()
    });
    let card_text = first.body["attachments"][0]["content"]["body"].to_string();
    assert!(
        card_text.contains("Hello, alice."),
        "card must render the narrate text; got: {}",
        first.body
    );
    let token = token_from_reply(&first.body);
    assert!(!token.is_empty());

    // Step 3a: Action.Execute invoke → refreshed card in the HTTP
    // response body (in-place refresh, no new message).
    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = client
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&invoke_activity(&token))
        .send()
        .await
        .expect("POST invoke");
    assert!(resp.status().is_success(), "{}", resp.status());
    let inv_body: Value = resp.json().await.expect("invoke response json");
    assert_eq!(
        inv_body["statusCode"], 200,
        "invoke response must be a 200 card action; got: {inv_body}"
    );
    assert_eq!(
        inv_body["type"], "application/vnd.microsoft.card.adaptive",
        "invoke response must return an adaptive card; got: {inv_body}"
    );
    let refreshed = inv_body["value"]["body"].to_string();
    assert!(
        refreshed.contains("Hello, alice."),
        "refreshed card must re-run narrate(alice); got: {inv_body}"
    );

    // Two dispatch audit lines: one inbound message + one callback.
    let dispatches = count_dispatches(&proc, "narrate");
    assert_eq!(
        dispatches, 2,
        "expected two narrate dispatch audit lines (inbound + Action.Execute)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_submit_callback_posts_a_reply() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let client = reqwest::Client::new();

    // Trigger the card, grab the token.
    let jwt = fake.sign_jwt(good_claims(&fake));
    client
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("/narrate bob"))
        .send()
        .await
        .expect("POST inbound");
    let first = wait_for(Duration::from_secs(3), || {
        fake.captured().into_iter().next()
    });
    let token = token_from_reply(&first.body);

    // Action.Submit → the adapter posts a NEW reply Activity back
    // through the bot connector (capture grows to 2).
    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = client
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&submit_activity(&token))
        .send()
        .await
        .expect("POST submit");
    assert!(resp.status().is_success(), "{}", resp.status());

    let two = wait_for(Duration::from_secs(3), || {
        let v = fake.captured();
        (v.len() >= 2).then_some(v)
    });
    let second = &two[1].body;
    let rendered = second["attachments"][0]["content"]["body"]
        .to_string()
        .to_lowercase()
        + second["text"].as_str().unwrap_or("");
    assert!(
        rendered.contains("hello, bob."),
        "Action.Submit must re-dispatch narrate(bob) and post the result; got: {second}"
    );
    assert_eq!(count_dispatches(&proc, "narrate"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_invoke_token_is_rejected() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // Token signed under a DIFFERENT key: HMAC verify must fail.
    let forged = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "evil" }),
        b"a-totally-different-key!!",
    )
    .expect("forged token fits");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&invoke_activity(&forged))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401, "forged invoke token must 401");

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:msteams"
    });
    let _ = rejected;
    assert!(
        fake.captured().is_empty(),
        "forged callback must not trigger any outbound reply"
    );
    assert_eq!(count_dispatches(&proc, "narrate"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_submit_token_is_rejected() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let forged = triton_correlation::encode(
        "narrate",
        &json!({ "subject": "evil" }),
        b"a-totally-different-key!!",
    )
    .expect("forged token fits");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&submit_activity(&forged))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401, "forged submit token must 401");

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["result"] == "error:auth"
            && v["protocol"] == "messenger:msteams"
    });
    let _ = rejected;
    assert!(fake.captured().is_empty());
    assert_eq!(count_dispatches(&proc, "narrate"), 0);
}

// ---- helpers -----------------------------------------------------

fn count_dispatches(proc: &TritonProcess, tool: &str) -> usize {
    // Give the binary a beat to flush audit lines.
    std::thread::sleep(Duration::from_millis(200));
    proc.stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["protocol"] == "messenger:msteams"
                && v["tool"] == tool
        })
        .count()
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

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe did not return Some within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
