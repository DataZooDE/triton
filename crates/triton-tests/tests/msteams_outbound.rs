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
fn conversation_reference(fake: &FakeBotFramework, tenant: &str) -> Value {
    json!({
        "service_url": fake.service_url(),
        "conversation_id": "a:conv-1",
        "bot_id": "28:bot-1",
        "user_id": "29:1abc",
        "tenant_id": tenant,
    })
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

/// A reference that names NO tenant must be refused.
///
/// `tenant_id` was `Option<String>`, and absent meant "no binding
/// asserted" — so omitting one field turned the cross-tenant check off
/// entirely. The reference is caller-supplied JSON, so the caller chose
/// whether to be bound. An `outbound:send` holder could deliver into any
/// conversation id it knew by leaving the field out.
///
/// The minting side (`conversation_reference_json`) always writes the
/// real tenant, so requiring it costs a genuine reference nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reference_without_a_tenant_is_forbidden() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    let mut reference = conversation_reference(&fake, "acme");
    reference.as_object_mut().unwrap().remove("tenant_id");

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(outbound_token(&issuer, "globex"))
        .json(&json!({
            "adapter": "msteams",
            "to": "29:1abc",
            "result": { "text": "unbound" },
            "reference": reference,
        }))
        .send()
        .await
        .expect("POST /v1/outbound");

    // 400, not 403, and the difference is the point: a reference with no
    // tenant is MALFORMED, while a reference naming someone else's tenant
    // is FORBIDDEN. Asserting the mode rather than "some 4xx" keeps the
    // two distinguishable — `cross_tenant_conversation_is_forbidden`
    // pins 403 for the other one.
    assert_eq!(
        resp.status(),
        400,
        "a reference with no tenant must be refused as malformed, not \
         treated as unbound — otherwise omitting a field disables the check"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "a forbidden outbound must not post to the connector"
    );
}

/// The destination host is caller-supplied, and the adapter POSTs there
/// with a real Bot Connector bearer for the app. Anything off Microsoft's
/// operated hosts is an exfiltration target.
///
/// `evil.trafficmanager.net` is the case that mattered: until #329 the
/// allowlist matched the whole Azure Traffic Manager namespace, which
/// anyone with a subscription can register a name in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attacker_controlled_service_url_is_forbidden() {
    let fake = FakeBotFramework::start().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer, &fake)).await;

    for host in [
        "https://evil.trafficmanager.net/",
        // Suffix confusion: ends with the magic string, wrong label boundary.
        "https://smba.trafficmanager.net.evil.example/",
        "https://attacker.example/",
        // Userinfo smuggling: the authority is the attacker's.
        "https://smba.trafficmanager.net@attacker.example/",
        // Wrong scheme.
        "http://smba.trafficmanager.net/",
    ] {
        let mut reference = conversation_reference(&fake, "acme");
        reference["service_url"] = json!(host);

        let resp = reqwest::Client::new()
            .post(proc.rest_url("/v1/outbound"))
            .bearer_auth(outbound_token(&issuer, "acme"))
            .json(&json!({
                "adapter": "msteams",
                "to": "29:1abc",
                "result": { "text": "exfil" },
                "reference": reference,
            }))
            .send()
            .await
            .expect("POST /v1/outbound");

        assert_eq!(
            resp.status(),
            403,
            "`{host}` must not receive the bot token"
        );
    }

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fake.captured().is_empty(),
        "no forbidden destination may reach a connector"
    );
}
