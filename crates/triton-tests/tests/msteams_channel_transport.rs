//! #250 (crew F3) — the declared channel must match the transport that
//! delivered the Activity.
//!
//! `allowed_channel_ids` gates which Bot Framework channels may assert an
//! Entra principal, and it reads `channelId` from the request BODY. The
//! body is unsigned. So a deployment that declares Teams only is still
//! reachable over any other channel Microsoft will mint a token for: the
//! attacker delivers over Direct Line — where `from.id` is client-chosen,
//! which is why `azure` refuses to BOOT on a declared Direct Line channel
//! — and simply writes `"channelId": "msteams"` in the body. The gate
//! sees the string it wanted. `from.aadObjectId` and
//! `channelData.tenant.id`, also unsigned, then become the principal.
//!
//! The one field that describes the TRANSPORT rather than the sender's
//! claim about it is `serviceUrl`, and on a multi-tenant bot Microsoft
//! signs it into the connector token. Microsoft assigns those hosts by
//! channel family: Teams lands on `smba.trafficmanager.net` (regionally —
//! `/amer/`, `/emea/`, `/in/`), Direct Line and WebChat on
//! `directline.botframework.com` / `webchat.botframework.com`. So a
//! signed `serviceUrl` from the Direct Line family alongside a body
//! claiming `msteams` is a contradiction, and the request is refused.
//!
//! **What this deliberately does NOT do.** The same crew finding asked
//! for `channelData.tenant.id` to be corroborated against `serviceUrl`
//! too. That is not achievable and the code should not pretend it is
//! pending: Microsoft assigns the host by REGION, and every tenant in a
//! region shares one. A host can never identify a tenant.
//!
//! It also cannot help a SINGLE-TENANT bot, where Entra signs the token
//! and no `serviceurl` claim is present at all — there the reply target
//! comes from the body as well, so corroborating one body field against
//! another proves nothing. That case is left explicitly uncorroborated
//! rather than given a check that looks like one.
//!
//! No mocks: real binary, real RS256 Bot Framework JWT verification.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";
const TENANT: &str = "acme-tenant-guid";

fn manifest_path(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("fixtures/{name}"))
        .display()
        .to_string()
}

fn env_with(fake: &FakeBotFramework, manifest: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path(manifest)),
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

/// Claims carrying a SIGNED `serviceurl` — the multi-tenant shape, the
/// only one where the transport is attested.
fn claims_with_service_url(service_url: &str) -> Value {
    json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": service_url,
    })
}

fn activity_on(channel_id: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg-1",
        "serviceUrl": "https://placeholder.example/",
        "channelId": channel_id,
        "from": { "id": "29:1abc", "name": "Alice",
                  "aadObjectId": "11111111-2222-3333-4444-555555555555" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "channelData": { "tenant": { "id": TENANT } },
        "text": "hello",
        "textFormat": "plain"
    })
}

async fn post_with(
    proc: &TritonProcess,
    fake: &FakeBotFramework,
    signed_service_url: &str,
    channel: &str,
) -> reqwest::StatusCode {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(claims_with_service_url(signed_service_url));
    reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity_on(channel))
        .send()
        .await
        .expect("POST")
        .status()
}

/// The hole: Direct Line transport, a body that says Teams.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_line_transport_cannot_claim_the_teams_channel() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-azure.yaml"),
    )
    .await;

    let status = post_with(
        &proc,
        &fake,
        "https://directline.botframework.com/",
        "msteams",
    )
    .await;

    assert_eq!(
        status, 401,
        "an Activity delivered on the Direct Line family must not be able \
         to claim `channelId: msteams` and mint an Entra principal from \
         unsigned body fields; got {status}"
    );
}

// There is deliberately no "Teams transport claiming `directline`" test.
// It cannot be written non-vacuously: `directline` has to be declared in
// `allowed_channel_ids` to reach the corroboration at all, and `azure`
// refuses to BOOT on a client-id channel (see
// `azure_identity_refuses_to_boot_on_a_client_id_channel`). Any such test
// would pass on the pre-existing channel gate and assert nothing about
// this one.

/// Teams over Teams keeps working — every documented region, so a new
/// one Microsoft adds cannot be refused by an over-narrow path match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_regions_are_all_accepted_as_teams_transport() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-azure.yaml"),
    )
    .await;

    for region in [
        "https://smba.trafficmanager.net/teams/",
        "https://smba.trafficmanager.net/amer/",
        "https://smba.trafficmanager.net/emea/",
        "https://smba.trafficmanager.net/in/",
        // A region that does not exist yet. The check is on the HOST
        // family; pinning the path would make every new Microsoft region
        // an outage.
        "https://smba.trafficmanager.net/antarctica/",
    ] {
        let status = post_with(&proc, &fake, region, "msteams").await;
        assert_ne!(
            status, 401,
            "`{region}` is Teams transport and must not be refused as a \
             channel contradiction"
        );
    }
}

/// A channel with no documented host family is left alone. Inventing a
/// mapping for `pva` (Copilot Studio) would refuse real traffic on a
/// guess — the same over-reach that made this finding a deferral in the
/// first place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undocumented_channel_family_is_not_corroborated() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-channels.yaml"),
    )
    .await;

    let status = post_with(&proc, &fake, "https://smba.trafficmanager.net/amer/", "pva").await;
    assert_ne!(
        status, 401,
        "`pva` has no documented serviceUrl family, so it must pass the \
         corroboration untouched rather than be refused on a guess"
    );
}
