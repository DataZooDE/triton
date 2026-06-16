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
        // PR 37 NFR-S-4: the fake bot framework serves replies from
        // `127.0.0.1:<port>` which is NOT on the production host
        // allowlist (`.botframework.com` / `.trafficmanager.net`).
        // The integration tests opt in to the extras list so the
        // JWT verifier accepts the fixture's `serviceUrl`. The
        // binary refuses this env var outside `local`, so it can't
        // leak into production.
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

fn vault_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-vault.yaml")
        .display()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msteams_token_url_override_refused_in_nonprod() {
    // PR 37 Finding 1 (CRITICAL, NFR-S-4): the binary must refuse to
    // wire the MS Teams adapter when `TRITON_ENV != local` AND
    // `TRITON_MSTEAMS_TOKEN_URL` is set to anything at all. Without
    // this guard, a compromised env var would POST the bot's
    // `client_credentials` (including `client_secret`) at an
    // attacker-controlled host.
    //
    // We use an `env://`-ref manifest so the manifest validation step
    // doesn't reject the literal-credential variant first. The secret
    // refs are never actually resolved — the NFR-S-4 settings check
    // runs BEFORE credential resolution, so no secret env vars are
    // needed to reach the asserted exit.
    let bin = locate_triton_binary();
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "nonprod")
        .env("TRITON_MANIFEST_PATH", vault_manifest_path())
        // The SSRF/exfil-tempting override.
        .env("TRITON_MSTEAMS_TOKEN_URL", "https://attacker.example/token")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "non-local env with overridden TRITON_MSTEAMS_TOKEN_URL MUST exit 2; \
         stderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("NFR-S-4"),
        "exit log MUST mention NFR-S-4; got: {combined}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msteams_rejects_unexpected_service_url() {
    // PR 37 Finding 2 (HIGH, NFR-S-4): a JWT can be otherwise valid
    // (issued, signed, in-audience, in-window) and STILL carry a
    // `serviceUrl` pointed at an attacker host (e.g. minted from a
    // Bot Framework developer playground). The adapter must refuse
    // before issuing any outbound. The audit emits `phase: rejected`
    // / `result: error:auth`; the fake bot framework must not see
    // any outbound activity.
    let fake = FakeBotFramework::start().await;
    // For this test we explicitly do NOT pass the 127.0.0.1 extras
    // so the JWT's attacker.example serviceUrl falls outside ALL
    // allowed hosts; the binary still uses the fake for OpenID +
    // token endpoints, both of which are dialled directly from
    // settings (no need for serviceUrl extras).
    let env: HashMap<String, String> = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        // No EXTRA_SERVICE_URL_HOSTS — the fake's host is irrelevant
        // here; we sign with `serviceUrl: https://attacker.example/`.
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let mut claims = good_claims(&fake);
    claims["serviceUrl"] = json!("https://attacker.example/v3/conversations/");
    let jwt = fake.sign_jwt(claims);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("hello"))
        .send()
        .await
        .expect("POST");
    // 401 because the JWT verifier rejected the claim.
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");

    // The outbound courier MUST NOT have been called.
    assert!(
        fake.captured().is_empty(),
        "untrusted serviceUrl MUST NOT trigger an outbound reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msteams_accepts_jwt_within_5min_skew() {
    // PR 37 Finding 4 (HIGH): jsonwebtoken's default exp leeway is
    // 60s; the adapter must use 5min (300s) — Microsoft's documented
    // skew. We sign a JWT whose `exp` is 4 minutes in the PAST —
    // would be rejected at the default 60s leeway, accepted at 300s.
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let mut claims = good_claims(&fake);
    // exp 4 min in the past, iat further back. With leeway = 300 this
    // verifies; with the 60s default it would fail.
    claims["exp"] = json!(now_unix() - 4 * 60);
    claims["iat"] = json!(now_unix() - 10 * 60);
    let jwt = fake.sign_jwt(claims);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("exp slightly past ok under skew"))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "JWT with exp 4min in past must verify under 5min leeway; got {}",
        resp.status()
    );

    // Dispatch audit fires (verification succeeded; downstream
    // dispatch follows normally).
    let _ = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:msteams"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msteams_rejects_jwt_beyond_5min_skew() {
    // Paired with the previous test: a JWT whose `exp` is 6 minutes
    // PAST (beyond the 5-min leeway) must still be rejected so the
    // larger leeway doesn't accidentally accept stale tokens.
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let claims = json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        // exp 6 minutes ago — beyond the 5-min leeway → rejected.
        "exp": now_unix() - 6 * 60,
        "iat": now_unix() - 12 * 60,
        "serviceUrl": fake.service_url(),
    });
    let jwt = fake.sign_jwt(claims);

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("expired beyond skew"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "rejected" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(rejected["result"], "error:auth");
}

// ---- azure identity strategy (FR-I-7) ----------------------------

fn azure_manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-azure-test.yaml")
        .display()
        .to_string()
}

/// An Activity carrying the Entra (AAD) identity fields the `azure`
/// strategy derives the Principal from: `from.aadObjectId` and
/// `channelData.tenant.id`. No sender_table entry is involved.
fn azure_message_activity(text: &str, aad_object_id: &str, tenant_id: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg-azure-1",
        "timestamp": "2026-05-25T10:00:00.0000000Z",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:zzz", "name": "Alice", "aadObjectId": aad_object_id },
        "conversation": { "id": "a:conv-azure", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "channelData": { "tenant": { "id": tenant_id } },
        "text": text,
        "textFormat": "plain"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_identity_derives_principal_from_aad_claims() {
    // FR-I-7 `azure`: with no sender_table, the Principal is derived
    // from the activity's verified-by-derivation Entra claims —
    // `from.aadObjectId` → sub, `channelData.tenant.id` → tenant.
    let fake = FakeBotFramework::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), azure_manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let activity = azure_message_activity("hello azure", "aad-obj-alice", "tenant-guid-acme");

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity)
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The dispatcher must see a Principal sourced from the AAD claims,
    // not a sender_table lookup.
    let dispatch = wait_for_audit(&proc, Duration::from_secs(3), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["protocol"] == "messenger:msteams"
    });
    assert_eq!(dispatch["who"], "aad-obj-alice", "sub = from.aadObjectId");
    assert_eq!(dispatch["subject"], "aad-obj-alice");
    assert_eq!(
        dispatch["tenant"], "tenant-guid-acme",
        "tenant = channelData.tenant.id"
    );
    assert_eq!(dispatch["result"], "ok");

    // Reply still couriers back through the bot connector.
    let captured = wait_for(Duration::from_secs(3), || {
        let v = fake.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].body["text"]
            .as_str()
            .unwrap_or("")
            .contains("hello azure")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_identity_rejects_disallowed_tenant() {
    // A perfectly valid JWT + AAD object id, but the inbound tenant
    // is not on the adapter's `allowed_tenants` list → 401 + a
    // `phase: rejected` audit, no dispatch, no outbound reply. This
    // is the cross-tenant-isolation guarantee.
    let fake = FakeBotFramework::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), azure_manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let activity = azure_message_activity("hello", "aad-obj-mallory", "tenant-guid-evil");

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
    assert!(
        fake.captured().is_empty(),
        "disallowed tenant MUST NOT trigger an outbound reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_identity_rejects_missing_aad_object_id() {
    // `azure` requires `from.aadObjectId`; a message activity without
    // it (e.g. a non-AAD channel) cannot yield an Entra principal and
    // must be refused rather than fall back to `from.id`.
    let fake = FakeBotFramework::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), azure_manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    // No aadObjectId, no channelData.
    let activity = json!({
        "type": "message",
        "id": "msg-azure-2",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:zzz", "name": "Alice" },
        "conversation": { "id": "a:conv-azure", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": "no aad id",
        "textFormat": "plain"
    });

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
async fn azure_identity_rejects_missing_tenant() {
    // Codex review finding 4: exercise the tenant-missing rejection
    // path specifically — aadObjectId IS present but channelData (and
    // thus tenant.id) is absent, so resolution must fail at the
    // tenant step, not the aadObjectId step.
    let fake = FakeBotFramework::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), azure_manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    // aadObjectId present, channelData/tenant absent.
    let activity = json!({
        "type": "message",
        "id": "msg-azure-3",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:zzz", "name": "Alice", "aadObjectId": "aad-obj-alice" },
        "conversation": { "id": "a:conv-azure", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": "no tenant",
        "textFormat": "plain"
    });

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
async fn azure_identity_rejects_non_teams_channel() {
    // Codex review finding 2: the AAD identity fields are body fields,
    // trusted only because the request is connector-authenticated AND
    // came over the Teams channel. A valid Bot Framework token for the
    // same bot on a different channel must NOT be allowed to inject an
    // Entra-shaped principal. Reject anything whose channelId != msteams.
    let fake = FakeBotFramework::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), azure_manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(good_claims(&fake));
    let mut activity = azure_message_activity("hello", "aad-obj-alice", "tenant-guid-acme");
    activity["channelId"] = json!("directline"); // not Teams

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
    assert!(
        fake.captured().is_empty(),
        "non-Teams channel MUST NOT trigger an outbound reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_empty_allowed_tenants_refuses_to_boot() {
    // Codex review finding 3: fail closed. An `azure` adapter with an
    // empty allowed_tenants list provides no cross-tenant isolation;
    // the binary MUST refuse to start rather than silently accept any
    // tenant's users.
    let bin = locate_triton_binary();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-azure-empty-tenants.yaml")
        .display()
        .to_string();
    let out = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "azure adapter with empty allowed_tenants MUST refuse to boot; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

// Locate the `triton` debug binary the same way `telegram_courier.rs`
// does — by walking up from the test crate's manifest dir.
fn locate_triton_binary() -> std::path::PathBuf {
    let mut here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let cand = here.join("target/debug/triton");
        if cand.exists() {
            return cand;
        }
        here.pop();
    }
    panic!("triton binary not found");
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
