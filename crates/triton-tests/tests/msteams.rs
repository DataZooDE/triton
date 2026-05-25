//! v0.2 PR 35 — Microsoft Teams adapter integration tests.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256
//! JWT verification against an in-repo `FakeBotFramework` fixture
//! that speaks the actual Bot Framework wire shape (OpenID
//! discovery + JWKS + OAuth2 token endpoint + reply Activity POST).
//!
//! Test matrix mirrors the spec from the parent task:
//!  * valid_jwt_message_dispatches_and_couriers
//!  * forged_jwt_signature_is_rejected
//!  * expired_jwt_is_rejected
//!  * wrong_audience_jwt_is_rejected
//!  * unknown_sender_rejected
//!  * mention_prefix_stripped_before_command_parse
//!  * non_message_activity_silently_acked
//!
//! Each test signs a fresh JWT inside the test so we can drive
//! the exp / aud / iss / serviceUrl axes independently.

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
        "timestamp": "2026-05-25T10:00:00.0000000Z",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": text,
        "textFormat": "plain"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_jwt_message_dispatches_and_couriers() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("hello from teams"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // 1. Dispatch audit fires.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(dispatch["tool"], "echo");
    assert_eq!(dispatch["tenant"], "acme");
    assert_eq!(dispatch["result"], "ok");

    // 2. Reply activity reaches the fake bot framework with a
    //    bearer access token attached.
    let captured = wait_for(Duration::from_secs(3), || {
        let v = fake.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1, "expected exactly one reply activity");
    let sent = &captured[0];
    assert_eq!(sent.conversation_id, "a:conv-1");
    assert!(
        sent.bearer.starts_with("Bearer "),
        "Authorization header must be a Bearer token; got: {}",
        sent.bearer
    );
    let text = sent.body["text"].as_str().expect("text in reply");
    assert!(
        text.contains("hello from teams"),
        "reply should echo back the inbound text; got: {text}"
    );

    // 3. Post audit fires with status_label=posted.
    let post_audit = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(post_audit["result"], "ok");
    assert_eq!(post_audit["status_label"], "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_jwt_signature_is_rejected() {
    // A JWT signed under a totally unrelated RSA key: the fixture's
    // JWKS doesn't contain a matching kid, so the verifier rejects
    // at the kid lookup step. Either way the request never reaches
    // the dispatcher.
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // Hand-roll a JWT with a kid that doesn't exist in the JWKS.
    // The header.alg is RS256 (what the fixture serves) but the
    // signature is over garbage — the verifier rejects on the
    // unknown-kid path before signature verification, which is the
    // same outcome as a forged signature.
    let header = json!({"alg":"RS256","typ":"JWT","kid":"definitely-not-a-real-key"});
    let payload = good_claims(&fake);
    let token = unsigned_jwt(&header, &payload);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&message_activity("forged"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");

    // No reply activity should have been emitted.
    assert!(
        fake.captured().is_empty(),
        "forged JWT must not trigger an outbound reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_jwt_is_rejected() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // exp in the past beyond jsonwebtoken's default 60s leeway.
    let expired = json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() - 600,
        "iat": now_unix() - 1200,
        "serviceUrl": fake.service_url(),
    });
    let jwt = fake.sign_jwt(expired);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("expired"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");
    assert!(fake.captured().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_audience_jwt_is_rejected() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let mut claims = good_claims(&fake);
    claims["aud"] = json!("different-app-id");
    let jwt = fake.sign_jwt(claims);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("wrong aud"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    // from.id absent from sender_table → 401 + error:auth audit.
    let mut activity = message_activity("hello");
    activity["from"]["id"] = json!("29:not-in-table");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");
    assert!(fake.captured().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_prefix_stripped_before_command_parse() {
    // Inbound text is `<at>@bot</at> /echo hello world`. The
    // adapter MUST strip the mention wrapper and route to the
    // `echo` tool with `{message: "hello world"}` — verified via
    // the reply activity body that the fake bot framework captures.
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let activity = message_activity("<at>@bot</at> /echo hello world");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity)
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    let captured = wait_for(Duration::from_secs(3), || {
        let v = fake.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1);
    let text = captured[0].body["text"].as_str().expect("text");
    assert!(
        text.contains("hello world"),
        "reply text should reflect the stripped command args; got: {text}"
    );
    assert!(
        !text.contains("<at>"),
        "mention wrapper must not survive into the reply: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_message_activity_silently_acked() {
    // type: "conversationUpdate" → 200, no dispatch, no rejection,
    // no outbound reply.
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let convo_update = json!({
        "type": "conversationUpdate",
        "id": "cu-1",
        "serviceUrl": fake.service_url(),
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" }
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&convo_update)
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // Give the binary a beat to be sure nothing dispatched.
    std::thread::sleep(Duration::from_millis(300));
    let dispatches: usize = proc
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:msteams"
        })
        .count();
    assert_eq!(dispatches, 0, "conversationUpdate MUST NOT dispatch");
    assert!(
        fake.captured().is_empty(),
        "conversationUpdate MUST NOT trigger an outbound reply"
    );
}

// ---- helpers -----------------------------------------------------

/// Encode a JWT with `alg: RS256` in the header and the given
/// payload, but a garbage signature segment. Used for the
/// forged-signature test where we want the verifier to refuse
/// before signature verification (no matching `kid` in JWKS).
fn unsigned_jwt(header: &Value, payload: &Value) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap());
    let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
    let sig = URL_SAFE_NO_PAD.encode(b"forged-signature-bytes-not-real-rs256");
    format!("{h}.{p}.{sig}")
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
