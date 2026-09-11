//! Out-of-band (proactive) Teams delivery via `POST /v1/outbound` — the write
//! half of async-operation delivery back to a chat channel.
//!
//! Unlike the inbound reply courier (`msteams_courier.rs`), nothing here is
//! handling a live turn: a caller submits a proactive send naming the `msteams`
//! adapter and carrying a persisted Teams `conversationReference` (its per-tenant
//! `serviceUrl`, `conversation.id`, bot + user ids, tenant). Triton resolves the
//! msteams `OutboundCourier`, renders the result, mints a Bot Connector token,
//! and POSTs the activity to the same conversation.
//!
//! No mocks: real spawned binary, real OIDC issuer (outbound audience + scope),
//! real HTTP to the in-repo `FakeBotFramework` (token endpoint + Bot Connector).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::{TestIssuer, TritonProcess};

const OUTBOUND_AUDIENCE: &str = "outbound-local";
const TRIO_AUDIENCE: &str = "agents-local";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-async-test.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(issuer: &TestIssuer, fake: &FakeBotFramework) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            TRIO_AUDIENCE.to_string(),
        ),
        (
            "TRITON_OUTBOUND_AUDIENCE".to_string(),
            OUTBOUND_AUDIENCE.to_string(),
        ),
    ])
}

fn outbound_token(issuer: &TestIssuer, tenant: &str) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "async-ops-courier",
        "aud": OUTBOUND_AUDIENCE,
        "exp": now() + 60,
        "iat": now(),
        "tenant": tenant,
        "scope": "outbound:send",
    }))
}

/// The persisted conversationReference the host would have captured on the
/// inbound turn and stored against the operation.
/// The key the fixture manifest gives the adapter.
const TEST_CORRELATION_KEY: &str = "correlation-key-for-test";

/// A reference as the ADAPTER would have minted it on an inbound turn:
/// sealed, so its fields are server-asserted rather than caller-written.
///
/// Holding the key here models the server, not an attacker — an attacker
/// has only whatever seal it was handed. `an_unsealed_reference_is_refused`
/// covers what happens without one.
fn conversation_reference(fake: &FakeBotFramework, tenant: &str) -> Value {
    sealed_reference(fake, tenant, "a:conv-1", "29:1abc")
}

fn sealed_reference(
    fake: &FakeBotFramework,
    tenant: &str,
    conversation_id: &str,
    user_id: &str,
) -> Value {
    let payload = json!({
        "service_url": fake.service_url(),
        "conversation_id": conversation_id,
        "bot_id": "28:bot-1",
        "user_id": user_id,
        "tenant_id": tenant,
    });
    let sealed = triton_correlation::encode_with_cap(
        "__conversation_ref",
        &payload,
        TEST_CORRELATION_KEY.as_bytes(),
        4096,
    )
    .expect("seal the reference");
    json!({ "channel": "msteams", "ref": sealed })
}

/// The happy path: a proactive msteams send renders + posts the activity to the
/// stored conversation on the real Bot Connector fake, and audits a `post`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proactive_msteams_send_delivers_to_the_stored_conversation() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "your scenario finished: reorder point is 118 units" },
            "reference": conversation_reference(&fake, "acme"),
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        202,
        "expected 202 Accepted, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );

    // The courier POSTed the activity to the SAME conversation on the connector.
    let sent = wait_for(Duration::from_secs(5), || {
        fake.captured()
            .into_iter()
            .find(|a| a.body["type"] == "message")
    });
    assert_eq!(sent.body["conversation"]["id"], "a:conv-1");
    assert!(
        sent.body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("reorder point is 118 units"),
        "the proactive activity carries the operation result; got: {}",
        sent.body
    );

    let post = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "post" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(post["result"], "ok");
    assert_eq!(post["status_label"], "posted");
}

/// #113 tenant binding: the caller's tenant must match the conversation's. A
/// token for `globex` may not deliver into an `acme` conversation — 403, and
/// nothing reaches the connector.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_tenant_conversation_is_forbidden() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "globex"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "cross-tenant" },
            "reference": conversation_reference(&fake, "acme"),
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert_eq!(
        resp.status(),
        403,
        "cross-tenant conversation must be forbidden"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "a forbidden outbound must not post to the connector"
    );
}

/// A proactive msteams send with NO reference fails closed (the recipient
/// identity is not a flat `to` for Teams) — never a silently dropped send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_without_a_conversation_reference_is_refused() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "no reference" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert!(
        !resp.status().is_success(),
        "a msteams outbound with no conversationReference must be refused, got {}",
        resp.status()
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "a refused outbound must not post to the connector"
    );
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
                "audit line not found within {deadline:?}; stdout:\n{}",
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
            panic!("probe timed out after {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// A seal this adapter did not mint is refused.
///
/// The seal is the whole trust boundary now, so the case that matters is
/// a reference that LOOKS sealed. Signed with another key, it must not
/// open — otherwise "sealed" would mean "has a `ref` field".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reference_sealed_with_another_key_is_refused() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let payload = json!({
        "service_url": fake.service_url(),
        "conversation_id": "a:conv-1",
        "bot_id": "28:bot-1",
        "user_id": "29:1abc",
        "tenant_id": "acme",
    });
    let forged = triton_correlation::encode_with_cap(
        "__conversation_ref",
        &payload,
        b"an-attackers-key-not-the-adapters",
        4096,
    )
    .expect("forge");

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "forged" },
            "reference": { "channel": "msteams", "ref": forged },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");

    assert_eq!(
        resp.status(),
        403,
        "a seal signed with another key must not open"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "nothing may reach the connector"
    );
}

/// A caller cannot redirect the post by naming a host.
///
/// The reply target used to be a caller-written field screened against
/// an allow-list — a screen worth getting right because the adapter POSTs
/// there with a real Bot Connector bearer. It now rides INSIDE the seal,
/// so a host written beside the seal is not read at all. The property is
/// no longer "the bad host is rejected" but "the caller's host is
/// irrelevant", which is the stronger one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_named_outside_the_seal_is_ignored() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let mut reference = conversation_reference(&fake, "acme");
    // Beside the seal, not inside it.
    reference["service_url"] = json!("https://evil.trafficmanager.net/");

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "where does this go?" },
            "reference": reference,
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert!(
        resp.status().is_success(),
        "the sealed send is valid: {}",
        resp.status()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let sent = loop {
        let c = fake.captured();
        if !c.is_empty() {
            break c;
        }
        if std::time::Instant::now() > deadline {
            panic!("the post went somewhere other than the sealed host");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(sent.len(), 1, "exactly one post, to the sealed host");
}

/// A stale conversation reference must be DROPPED, not retried forever.
///
/// Teams answers 403/404 when the bot has been removed from a
/// conversation, and for proactive delivery a stale reference is the
/// expected steady state, not an exception. The courier marked EVERY
/// non-2xx `PostOutcome::Retry` — including those — while the inbound
/// reply path already classified correctly (`>= 500 || 429` retry, else
/// drop). Same connector, same failures, two answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_conversation_is_dropped_not_retried() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    // The bot is no longer in this conversation.
    fake.set_activity_status(403);

    let _ = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "into a conversation the bot has left" },
            "reference": conversation_reference(&fake, "acme"),
        }))
        .send()
        .await
        .expect("POST /v1/outbound");

    let line = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "post"
    });
    assert_eq!(
        line["status_label"].as_str().unwrap_or_default(),
        "dropped",
        "a 403 from the connector means the reference is stale — retrying \
         it forever is the one answer that cannot help; got {line}"
    );
}

/// A proactive send must carry the buttons its result carries.
///
/// This feature exists to deliver a FINISHED long-running operation back
/// to the conversation that started it — and the archetypal such result
/// is one asking for approval. The courier rendered through
/// `text_reply_message`, which drops every interactive component, so the
/// approval prompt arrived as prose with nothing to click. Worse, a
/// render error became the literal text `(no content)` posted with
/// `result: ok`.
///
/// `build_reply_body` is the inbound path's renderer and already binds
/// each control's correlation token to (tenant, recipient), which is
/// what keeps a proactively-delivered card from being a capability for
/// whoever can see it (#250/#287).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proactive_send_delivers_the_buttons_the_result_carries() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "surface": { "components": [
                { "kind": "text", "value": "The migration finished. Approve the cutover?" },
                { "kind": "button", "label": "Approve cutover",
                  "tool": "assistant", "args": { "message": "approve" } }
            ] } },
            "reference": conversation_reference(&fake, "acme"),
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert!(resp.status().is_success(), "send failed: {}", resp.status());

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let sent = loop {
        let c = fake.captured();
        if !c.is_empty() {
            break c;
        }
        if std::time::Instant::now() > deadline {
            panic!("nothing reached the connector");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let body = serde_json::to_string(&sent[0].body).expect("serialise activity");

    assert!(
        body.contains("Approve cutover"),
        "the approval button must survive proactive delivery — an approval \
         request with nothing to click is the case this feature exists for; \
         got {body}"
    );
    // The button must carry a token that DECODES against the recipient's
    // binding — that is what proves it went through the signing path.
    //
    // This assertion used to be `body.contains("ct")`, which can never
    // fail: "ct" is a substring of "Action" and "actions", both present
    // in every Adaptive Card. It passed on a card with no token at all
    // (verified). I described it as proof; it was decoration.
    let token = sent[0].body["attachments"][0]["content"]["actions"]
        .as_array()
        .and_then(|a| a.iter().find_map(|x| x["data"]["ct"].as_str()))
        .unwrap_or_else(|| panic!("no correlation token on the delivered control: {body}"));
    let decoded = triton_correlation::decode_bound_any(
        token,
        &triton_correlation::KeyRing::single(TEST_CORRELATION_KEY.as_bytes()).expect("key ring"),
        4096,
        triton_correlation::Binding {
            platform: "msteams",
            tenant: "acme",
            sender: "29:1abc",
        },
    );
    assert!(
        decoded.is_ok(),
        "the token must decode against (tenant acme, recipient 29:1abc) — \
         a proactively delivered card is a capability for its recipient \
         and nobody else; got {decoded:?}"
    );
    assert!(
        !body.contains("(no content)"),
        "a render failure must not be posted as the literal text \
         `(no content)`; got {body}"
    );
}

/// The outbound audit line must name WHERE the push went.
///
/// For an inbound turn the destination is implied — the reply goes back
/// to the conversation the request came from, and `trace_id` ties the
/// two together. An agent-initiated push has no inbound turn, so the
/// record said a send happened, to whom it was attributed, and nothing
/// about which conversation received it. That is the one question
/// forensics asks first about a proactive message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outbound_audit_names_its_destination() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "acme"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "the operation finished" },
            "reference": conversation_reference(&fake, "acme"),
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    assert!(resp.status().is_success(), "send failed: {}", resp.status());

    let line = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "post"
    });
    assert_eq!(
        line["destination"].as_str().unwrap_or_default(),
        "a:conv-1",
        "the post record must name the conversation it reached; got {line}"
    );
}

/// The tenant binding must bind the CONVERSATION, not the caller to
/// itself.
///
/// `authorize` compares the reference's `tenant_id` with the caller's
/// tenant — but the caller writes BOTH the reference's tenant and its
/// conversation id. Requiring `tenant_id` removed the "leave it out"
/// route and nothing else: a globex caller can still write
/// `tenant_id: "globex"` next to an ACME conversation id and pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_self_asserted_tenant_does_not_unlock_another_tenants_conversation() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    // Hand-written, exactly as a caller would: its own tenant beside
    // someone else's conversation. Under the old contract this passed
    // with 202, because `authorize` compared two fields the caller wrote.
    let reference = json!({
        "channel": "msteams",
        "service_url": fake.service_url(),
        "conversation_id": "a:acme-private-thread",
        "bot_id": "28:bot-1",
        "user_id": "29:1abc",
        "tenant_id": "globex",
    });

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "globex"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "into someone else's conversation" },
            "reference": reference,
        }))
        .send()
        .await
        .expect("POST /v1/outbound");

    // 400: a hand-written reference carries no seal at all, so it is
    // MALFORMED rather than forbidden. `a_reference_sealed_with_another_key_is_refused`
    // pins 403 for the case that looks like a seal and is not.
    assert_eq!(
        resp.status(),
        400,
        "an unsealed reference must be refused outright: its fields are \
         caller-written, so nothing in it can bind anything"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "nothing may reach the connector"
    );
}
