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
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::{TritonProcess, locate_triton_binary};

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

/// `(status, body)`. The body matters: 401 is also what a wrong channel,
/// a missing `aadObjectId`, a disallowed tenant and a rejected
/// `serviceUrl` return, so a bare status assertion cannot tell which
/// check fired — today's 401 would keep passing tomorrow for the wrong
/// reason.
async fn post_with(
    proc: &TritonProcess,
    fake: &FakeBotFramework,
    signed_service_url: &str,
    channel: &str,
) -> (reqwest::StatusCode, String) {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(claims_with_service_url(signed_service_url));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity_on(channel))
        .send()
        .await
        .expect("POST");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    (status, body)
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

    let (status, body) = post_with(
        &proc,
        &fake,
        "https://directline.botframework.com/",
        "msteams",
    )
    .await;

    assert_eq!(
        (status.as_u16(), body.as_str()),
        (401, "channel mismatch"),
        "an Activity delivered on the Direct Line family must not be able \
         to claim `channelId: msteams` and mint an Entra principal from \
         unsigned body fields — and must be refused by THIS check, not \
         incidentally by another 401"
    );
}

// There is deliberately no "Teams transport claiming `directline`" test.
// It cannot be written non-vacuously: `directline` has to be declared in
// `allowed_channel_ids` to reach the corroboration at all, and `azure`
// refuses to BOOT on a client-id channel (see
// `azure_identity_refuses_to_boot_on_a_client_id_channel`). Any such test
// would pass on the pre-existing channel gate and assert nothing about
// this one.

/// The bypass a crew review found in the first cut of this control.
///
/// `allowed_channel_ids` is lowercased at build time and compared with
/// `eq_ignore_ascii_case`, so `"MSTEAMS"` passes the gate. But
/// `expected_service_url_family` matched the RAW `channelId` against
/// exact lowercase literals, returned `None`, and the `let … else` chain
/// short-circuited — skipping the corroboration entirely. One character
/// of case reopened the exact path this file exists to close.
///
/// The two comparisons have to agree on the same folded value, and the
/// only way to keep them agreeing is to fold ONCE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upper_case_channel_id_cannot_skip_the_corroboration() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-azure.yaml"),
    )
    .await;

    let (status, body) = post_with(
        &proc,
        &fake,
        "https://directline.botframework.com/",
        "MSTEAMS",
    )
    .await;

    assert_eq!(
        (status.as_u16(), body.as_str()),
        (401, "channel mismatch"),
        "`MSTEAMS` must be corroborated exactly like `msteams`; a case \
         change must not turn the channel into one with no documented \
         family and skip the check"
    );
}

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
        let (status, body) = post_with(&proc, &fake, region, "msteams").await;
        // `assert_ne!(status, 401)` would also pass on a 400, a 500 or a
        // panic-to-502 — i.e. on a regression that breaks Teams dispatch
        // outright. Assert the success the sibling happy-path tests
        // establish for this body shape.
        assert_eq!(
            status, 200,
            "`{region}` is Teams transport and must dispatch normally; got \
             {status}: {body}"
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

    let (status, body) =
        post_with(&proc, &fake, "https://smba.trafficmanager.net/amer/", "pva").await;
    assert_eq!(
        status, 200,
        "`pva` has no documented serviceUrl family, so it must pass the \
         corroboration untouched rather than be refused on a guess; got \
         {status}: {body}"
    );
}

/// The exemption's load-bearing premise, which rested on one untested
/// `exit(2)`.
///
/// `is_extra_service_url_host` skips the corroboration for hosts named in
/// `TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS`, and that is only safe
/// because a non-`local` deployment cannot set them: `triton-bin`'s
/// wiring refuses to boot. Three reviewers verified that by reading the
/// code, which is exactly the kind of assurance a refactor erases without
/// anyone noticing. Assert it instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_service_url_hosts_refuse_to_boot_outside_local() {
    let bin = locate_triton_binary();
    let mut child = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        // The whole point: NOT `local`.
        .env("TRITON_ENV", "nonprod")
        .env(
            "TRITON_MANIFEST_PATH",
            manifest_path("manifest-msteams-azure-envref.yaml"),
        )
        // The full well-known URL: a non-`local` env has its own guard on
        // this one, and it fires FIRST. Without it this test exits 2 for
        // the wrong reason — which it did, twice, before the assertion on
        // the message below caught it.
        .env(
            "TRITON_MSTEAMS_OPENID_URL",
            "https://login.botframework.com/v1/.well-known/openidconfiguration",
        )
        // Satisfy the `env://` refs so the manifest validator passes and
        // the boot reaches the guard under test. The literal-credential
        // fixture exits earlier, for an unrelated reason — which is how
        // the first version of this test "passed".
        .env("MSTEAMS_TEST_AUDIENCE", "triton-msteams-test-appid")
        .env("MSTEAMS_TEST_CLIENT_ID", "triton-msteams-test-appid")
        .env("MSTEAMS_TEST_CLIENT_SECRET", "client-secret-for-test")
        .env("MSTEAMS_TEST_CORRELATION_KEY", "correlation-key-for-test")
        .env(
            "MSTEAMS_TEST_AZURE_IDENTITY",
            r#"{"allowed_tenants":["acme-tenant-guid"],"scopes":["chat"]}"#,
        )
        .env("TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS", "127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                panic!(
                    "a non-`local` env with TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS \
                     set MUST refuse boot — the channel-corroboration exemption \
                     depends on it — but the binary kept running"
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(!status.success(), "must exit non-zero; got {status:?}");

    let mut out = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut out);
    }
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read;
        let _ = o.read_to_string(&mut out);
    }
    assert!(
        out.contains("EXTRA_SERVICE_URL_HOSTS"),
        "the refusal must name the variable an operator has to remove; \
         got: {out}"
    );
}

/// The control announces when it is NOT protecting anything.
///
/// The corroboration needs an ATTESTED `serviceUrl`, and a single-tenant
/// Entra bot's token carries no `serviceurl` claim — the shape agent-lab
/// runs. On such a deployment this check never fires, while the code and
/// its tests read as though the hole is closed. That is the
/// false-confidence risk a crew review named, so the adapter says so on
/// the first Activity that skips the check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unattested_activity_says_the_corroboration_did_not_apply() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-azure.yaml"),
    )
    .await;

    // No `serviceurl` claim in the token: the single-tenant shape. The
    // body still carries one, so the request is servable.
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
    }));
    let mut activity = activity_on("msteams");
    activity["serviceUrl"] = json!(fake.service_url());
    let _ = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity)
        .send()
        .await
        .expect("POST");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let said = loop {
        if proc
            .stdout_snapshot()
            .iter()
            .any(|l| l.contains("corroboration INACTIVE"))
        {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        said,
        "an Activity with no signed `serviceurl` must make the adapter \
         announce that the corroboration did not apply — otherwise a \
         deployment where this control never fires looks exactly like one \
         where it does"
    );
}

/// F11 — does a sender-table deployment make ANY channel trust decision?
///
/// `allowed_channel_ids` and the #319 corroboration both live inside the
/// `IdentityMode::Azure` arm. `IdentityMode::SenderTable` resolves from
/// `from.id` alone. A crew security seat raised the consequence without
/// verifying it: on a channel where the CLIENT chooses `from.id` — Direct
/// Line, Web Chat — a valid Bot Framework token for this bot would then
/// resolve to whatever principal the table maps that id to.
///
/// This test established reachability rather than assuming it, and the
/// answer is YES: the adapter dispatched as `alice` with status 200 for
/// an Activity declaring `channelId: "directline"`. The fixture maps
/// `29:1abc` → `alice`/`acme`; the Activity below claims that id.
///
/// IGNORED because it documents a gap that is not fixed yet, not a
/// regression. Closing it is a config decision — a channel gate hoisted
/// above the identity-mode match, or channel-qualified sender-table keys
/// — either of which changes the manifest surface and every existing
/// table. Whoever takes that on has their red test here: remove the
/// `#[ignore]`.
///
/// The remaining unknown is not the code path but the Azure side: an
/// attacker still needs a valid Bot Framework token for THIS bot, which
/// means the bot's registration must have a client-chosen-`from.id`
/// channel enabled. That is a fact about the deployment, not the repo.
#[ignore = "F11: confirmed gap — sender_table applies no channel gate; fix is a manifest decision"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sender_table_makes_no_channel_trust_decision() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-test.yaml"),
    )
    .await;

    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(claims_with_service_url(&fake.service_url()));
    let mut activity = activity_on("directline");
    // The mapped Teams sender id, claimed from a Direct Line-shaped
    // delivery. On Direct Line this field is client-chosen.
    activity["from"]["id"] = json!("29:1abc");
    activity["serviceUrl"] = json!(fake.service_url());

    let status = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity)
        .send()
        .await
        .expect("POST")
        .status();

    // Did it dispatch as `alice`?
    let dispatched_as_alice = {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let hit = proc.stdout_snapshot().iter().any(|l| {
                l.contains("\"phase\":\"dispatch\"") && l.contains("\"subject\":\"alice\"")
            });
            if hit {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    assert!(
        !dispatched_as_alice,
        "a sender-table adapter dispatched as `alice` for an Activity \
         declaring `channelId: directline` — the channel is never checked \
         in this identity mode, so a client-chosen `from.id` on a \
         Direct Line-family channel becomes a mapped principal. \
         (status was {status})"
    );
}
