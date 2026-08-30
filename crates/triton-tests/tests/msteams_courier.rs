//! #635 P4 — msteams async reply courier + Teams streaming shell.
//!
//! The inline path holds the inbound webhook connection across the
//! whole dispatch → Connector-POST chain; Bot Framework abandons it at
//! ~15s and hyper then DROPS the handler future, killing the reply
//! after the dispatch already succeeded (observed live 2026-08-30:
//! `dispatch ok latency_ms=23460`, no post record, ingress 499).
//! With `TRITON_MSTEAMS_ASYNC` the webhook acks immediately and a
//! spawned task delivers out-of-band:
//!
//!   * personal (1:1) chats — the Teams streaming shell: an
//!     informative `typing` activity opens a stream, the final
//!     `message` closes it (`streamType: final`, no sequence);
//!   * group chats / unknown — plain typing activity + a normal
//!     proactive message (streaming is 1:1-only on the platform).
//!
//! No mocks: a real spawned binary against the in-repo
//! `FakeBotFramework` (real axum server, real TCP).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-async-test.yaml")
        .display()
        .to_string()
}

fn courier_env(fake: &FakeBotFramework, upstream: &FakeAgent) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        ("TRITON_MSTEAMS_ASYNC".to_string(), "1".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("answer={}", upstream.host_port()),
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
        "serviceurl": fake.service_url(),
    })
}

fn message_activity(text: &str, conversation_type: Option<&str>) -> Value {
    let mut conversation = json!({ "id": "a:conv-1" });
    if let Some(ct) = conversation_type {
        conversation["conversationType"] = json!(ct);
    }
    json!({
        "type": "message",
        "id": "msg-1",
        "timestamp": "2026-05-25T10:00:00.0000000Z",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": conversation,
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": text,
        "textFormat": "plain"
    })
}

/// 1:1 chat, slow (2s) dispatch: the ack must not wait, the stream
/// must open with an informative update BEFORE the answer exists, and
/// the final message must close the stream carrying the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn personal_chat_acks_fast_and_streams_informative_then_final() {
    let fake = FakeBotFramework::start().await;
    let upstream = FakeAgent::start_returning_after(
        Duration::from_secs(2),
        json!({ "answer": "42, after a long think" }),
    )
    .await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), courier_env(&fake, &upstream)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(good_claims(&fake));

    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("what is the answer?", Some("personal")))
        .send()
        .await
        .expect("POST");
    let ack_latency = started.elapsed();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert!(
        ack_latency < Duration::from_millis(500),
        "courier ack must not wait for the (2s) dispatch; took {ack_latency:?}"
    );

    // Two activities, in order: the stream-opening informative typing
    // activity, then the final message closing the stream.
    let captured = wait_for(Duration::from_secs(5), || {
        let v = fake.captured();
        (v.len() >= 2).then_some(v)
    });
    let opener = &captured[0];
    assert_eq!(opener.body["type"], "typing");
    assert_eq!(
        opener.body["entities"][0]["type"], "streaminfo",
        "the opener must carry the streaminfo entity; got: {}",
        opener.body
    );
    assert_eq!(opener.body["entities"][0]["streamType"], "informative");
    assert_eq!(opener.body["entities"][0]["streamSequence"], 1);
    assert!(
        opener.body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Working on it"),
        "start activities require text; got: {}",
        opener.body
    );

    let final_msg = &captured[1];
    assert_eq!(final_msg.body["type"], "message");
    assert_eq!(final_msg.body["entities"][0]["streamType"], "final");
    assert_eq!(
        final_msg.body["entities"][0]["streamId"], "stub-activity-id",
        "the final must address the stream the opener's response id named"
    );
    assert!(
        final_msg.body["entities"][0]
            .get("streamSequence")
            .is_none(),
        "a final carries NO streamSequence; got: {}",
        final_msg.body
    );
    assert!(
        final_msg.body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("42, after a long think"),
        "got: {}",
        final_msg.body
    );

    // One audited post, real status.
    let post = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(post["result"], "ok");
    assert_eq!(post["status_label"], "posted");
    assert_eq!(post["status"], 200);
}

/// Group chat: streaming is illegal, so the courier keeps a typing
/// ticker alive during the dispatch and delivers ONE normal message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_chat_types_then_posts_a_plain_message() {
    let fake = FakeBotFramework::start().await;
    let upstream = FakeAgent::start_returning_after(
        Duration::from_secs(3),
        json!({ "answer": "group answer" }),
    )
    .await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), courier_env(&fake, &upstream)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(good_claims(&fake));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("hello group", Some("groupChat")))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The final message eventually lands…
    let captured = wait_for(Duration::from_secs(6), || {
        let v = fake.captured();
        v.iter().any(|a| a.body["type"] == "message").then_some(v)
    });
    // …preceded by at least one plain typing activity (3s dispatch at a
    // 2.5s cadence guarantees one), and NO streaminfo anywhere.
    let typings: Vec<_> = captured
        .iter()
        .filter(|a| a.body["type"] == "typing")
        .collect();
    assert!(
        !typings.is_empty(),
        "the ticker must keep the indicator alive during dispatch"
    );
    for a in &captured {
        assert!(
            a.body.get("entities").is_none(),
            "no streaminfo outside 1:1 chats; got: {}",
            a.body
        );
    }
    let final_msg = captured
        .iter()
        .find(|a| a.body["type"] == "message")
        .unwrap();
    assert!(
        final_msg.body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("group answer"),
        "got: {}",
        final_msg.body
    );
}

/// No conversationType at all (defensive: not every channel sends it):
/// must behave exactly like a group — the conservative branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_conversation_type_is_treated_as_group() {
    let fake = FakeBotFramework::start().await;
    let upstream = FakeAgent::start_returning(json!({ "answer": "typed-unknown answer" })).await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), courier_env(&fake, &upstream)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(good_claims(&fake));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("hello unknown", None))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    let captured = wait_for(Duration::from_secs(5), || {
        let v = fake.captured();
        v.iter().any(|a| a.body["type"] == "message").then_some(v)
    });
    for a in &captured {
        assert!(
            a.body.get("entities").is_none(),
            "unknown conversationType must NOT stream; got: {}",
            a.body
        );
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
