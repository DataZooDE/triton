//! Proactive `POST /v1/outbound` on a WhatsApp adapter whose identity is
//! `upstream` — the tenant binding the sender-table adapters have always had.
//!
//! Under `sender_table`, `authorize` looks the recipient up in an
//! operator-enumerated table and refuses unless `claims.tenant` equals the
//! caller's. Under `upstream` there is no table, and the courier used to
//! answer `Ok(())` for every send. The comment on it explained why that was
//! acceptable: the caller's `tenant` is verified, and the endpoint is gated by
//! a dedicated audience + scope.
//!
//! That reasoning held while `/v1/outbound` was the only door. It stopped
//! holding when the async-ops delivery callback arrived: that receiver builds
//! the delivery `Principal` in process from the callback body — no OIDC, no
//! audience, no scope — so the gate the comment leans on is not in that path.
//! A courier that binds nothing is then the only thing between a caller-named
//! tenant and someone else's conversation.
//!
//! What this mode binds is RESOLVABILITY, not tenant equality: the recipient
//! must be someone the resolver knows. Equality would be wrong here — the
//! resolver may legitimately place a recipient in a different tenant from the
//! caller's, and #287 depends on exactly that. An empty caller tenant is
//! refused in both modes, because `""` is not a tenant.
//!
//! No mocks: real spawned binary, real OIDC issuer, real HTTP to the in-repo
//! `FakeAgent` resolver and `FakeWhatsAppApi`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use triton_tests::chat_courier_fixture::FakeWhatsAppApi;
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const OUTBOUND_AUDIENCE: &str = "outbound-local";
const APP_SECRET: &str = "whatsapp-app-secret-for-test";
/// Not in any sender table — only the resolver knows it, and the resolver
/// places it in `globex`.
const UNKNOWN_WA_ID: &str = "490000000001";
const RESOLVED_TENANT: &str = "globex";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-whatsapp-cloud-upstream-identity.yaml")
        .display()
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn env_for(
    issuer: &TestIssuer,
    whatsapp: &FakeWhatsAppApi,
    resolver: &FakeAgent,
    agent: &FakeAgent,
) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_WHATSAPP_API_BASE".to_string(), whatsapp.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "assistant={},resolve_identity={}",
                agent.host_port(),
                resolver.host_port()
            ),
        ),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "agents-local".to_string(),
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
        "sub": "some-agent",
        "aud": OUTBOUND_AUDIENCE,
        "exp": now() + 60,
        "iat": now(),
        "tenant": tenant,
        "scope": "outbound:send",
    }))
}

fn resolver_returning(tenant: &str) -> Value {
    json!({ "sub": "resolved-bob", "scopes": ["chat"], "tenant": tenant })
}

fn sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn inbound_envelope(wa_id: &str, text: &str) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "0", "changes": [{ "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15555555555", "phone_number_id": "100200300" },
            "messages": [{ "from": wa_id, "id": "wamid.X", "timestamp": "1700000000",
                "type": "text", "text": { "body": text } }]
        }, "field": "messages" }] }]
    })
}

/// Open the 24-hour service window for `wa_id`. Without an inbound first, a
/// plain-text proactive send is refused 400 ("a template `category` is
/// required") BEFORE the courier authorizes — which would make every
/// assertion below pass for a reason that has nothing to do with tenants.
async fn open_service_window(proc: &TritonProcess, wa_id: &str) {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener bound");
    let body = serde_json::to_vec(&inbound_envelope(wa_id, "hello")).unwrap();
    let sig = sign(&body, APP_SECRET);
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/whatsapp/webhook"))
        .header("X-Hub-Signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST inbound webhook");
    assert!(
        resp.status().is_success(),
        "inbound seed: {}",
        resp.status()
    );
}

async fn send(proc: &TritonProcess, token: &str, to: &str) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(token)
        .json(&json!({
            "adapter": "whatsapp",
            "to": to,
            "result": { "text": "your scenario finished" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

async fn send_with_category(proc: &TritonProcess, token: &str, to: &str) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/outbound"))
        .bearer_auth(token)
        .json(&json!({
            "adapter": "whatsapp",
            "to": to,
            "category": "utility",
            "result": { "text": "your scenario finished" },
        }))
        .send()
        .await
        .expect("POST /v1/outbound");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(start.elapsed() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// POSITIVE CONTROL: a caller in the recipient's own tenant may send, and the
/// message really reaches the platform. Without this, the refusals below could
/// be the adapter rejecting every proactive send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_in_the_recipients_tenant_may_send() {
    let resolver = FakeAgent::start_returning(resolver_returning(RESOLVED_TENANT)).await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let agent = FakeAgent::start_echoing().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&issuer, &whatsapp, &resolver, &agent),
    )
    .await;

    open_service_window(&proc, UNKNOWN_WA_ID).await;
    // The adapter couriers an inbound REPLY of its own; wait for it and take
    // its count as the baseline, so what is asserted below is the proactive
    // send rather than the reply that opened the window.
    let baseline = wait_for(Duration::from_secs(5), || {
        let n = whatsapp.captured().len();
        (n > 0).then_some(n)
    });

    let (status, body) = send(
        &proc,
        &outbound_token(&issuer, RESOLVED_TENANT),
        UNKNOWN_WA_ID,
    )
    .await;
    assert_eq!(status, 202, "same-tenant send must be accepted: {body}");

    let sent = wait_for(Duration::from_secs(5), || {
        let v = whatsapp.captured();
        (v.len() > baseline).then_some(v)
    });
    assert_eq!(
        sent[baseline].body["to"], UNKNOWN_WA_ID,
        "the proactive send reached the platform"
    );
}

/// Cross-tenant IS allowed here, and deliberately so: the resolver, not the
/// caller's tenant, says who may be messaged, and `outbound_mint_tenant` then
/// mints the button token for the RECIPIENT's tenant because the recipient is
/// who taps it. That property is pinned by
/// `outbound::outbound_buttons_are_minted_for_the_recipients_tenant` (#287),
/// which a tenant-equality rule here would break — it was written with the
/// resolver answering a tenant different from the caller's on purpose.
///
/// A resolver that cannot answer must fail the send closed. An unresolvable
/// recipient has no tenant to bind against, and guessing one is the failure
/// mode this whole binding exists to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unresolvable_recipient_is_refused() {
    let resolver = FakeAgent::start_always_failing().await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let agent = FakeAgent::start_echoing().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&issuer, &whatsapp, &resolver, &agent),
    )
    .await;

    // A failing resolver also rejects the INBOUND (401), so the window cannot
    // be opened through it. Send with an explicit template `category` instead:
    // that is the path a proactive send takes outside the window, and it still
    // runs the courier's authorize.
    let (status, body) = send_with_category(
        &proc,
        &outbound_token(&issuer, RESOLVED_TENANT),
        UNKNOWN_WA_ID,
    )
    .await;
    assert_eq!(
        status, 403,
        "an unresolvable recipient has no tenant to bind against: {body}"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        whatsapp.captured().is_empty(),
        "nothing may reach the platform when the recipient cannot be resolved"
    );
}

/// An EMPTY caller tenant matches nothing. The sender-table couriers fail
/// closed here only because no enrolled entry carries an empty tenant — true
/// today, and an unasserted property of operator data rather than of the code.
/// Under `upstream` the resolver's answer is never empty either. Pin it: the
/// delivery principal the async-ops callback builds carries exactly this shape
/// when an operation records no `channel_tenant` (DataZooDE/triton#332).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_caller_tenant_is_refused() {
    let resolver = FakeAgent::start_returning(resolver_returning(RESOLVED_TENANT)).await;
    let whatsapp = FakeWhatsAppApi::start().await;
    let agent = FakeAgent::start_echoing().await;
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_for(&issuer, &whatsapp, &resolver, &agent),
    )
    .await;

    open_service_window(&proc, UNKNOWN_WA_ID).await;
    let baseline = wait_for(Duration::from_secs(5), || {
        let n = whatsapp.captured().len();
        (n > 0).then_some(n)
    });

    let (status, body) = send(&proc, &outbound_token(&issuer, ""), UNKNOWN_WA_ID).await;
    // 403 specifically, and from the courier's own comparison — not a 401
    // from the auth layer, which would make this test say nothing about the
    // binding it is here to pin.
    assert_eq!(
        status, 403,
        "an empty tenant must be refused by the courier: {body}"
    );
    assert!(
        body.contains("is not in tenant"),
        "the refusal must come from the tenant comparison: {body}"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        whatsapp.captured().len(),
        baseline,
        "nothing may reach the platform for an empty-tenant caller"
    );
}
