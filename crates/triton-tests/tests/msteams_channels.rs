//! #250 — which Bot Framework channels may assert an Entra principal.
//!
//! The `azure` identity strategy gated on a hardcoded
//! `channelId == "msteams"`. The gate is sound — a valid token for this
//! bot arriving over another channel must not inject an Entra-shaped
//! principal — but a hardcoded literal makes every other Bot Framework
//! channel unreachable, which is exactly why Copilot Studio, WebChat and
//! M365 Copilot Chat cannot be served today.
//!
//! Two crew reviews rejected the alternative (gating on the JWKS
//! `endorsements` field): endorsements exist only on the Bot Framework
//! keyset, and a single-tenant bot — the type Microsoft now requires,
//! and the one `agent-lab` runs — is signed by the Entra anchor, which
//! publishes none. An endorsement gate would refuse every live request
//! or, carved out, delete the channel gate and replace it with nothing.
//!
//! So the channel set becomes an explicit operator declaration. Not a
//! widening: the deployment states which channels it serves, unknown
//! ones are still refused, and the trust argument (connector-
//! authenticated body metadata on a channel we chose to trust) is
//! unchanged — it is merely written down instead of compiled in.
//!
//! No mocks per CLAUDE.md §1: real binary, real RS256 Bot Framework JWT
//! verification against the in-repo `FakeBotFramework`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn good_claims(fake: &FakeBotFramework) -> Value {
    json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
    })
}

/// An Activity on `channel_id` carrying the Entra body fields the
/// `azure` strategy reads.
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

async fn post(proc: &TritonProcess, fake: &FakeBotFramework, channel: &str) -> reqwest::StatusCode {
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(good_claims(fake));
    reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&activity_on(channel))
        .send()
        .await
        .expect("POST")
        .status()
}

/// The functional gap: a channel the operator has declared is served.
/// `pva` is the Bot Framework channel id for Power Virtual Agents /
/// Copilot Studio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_non_teams_channel_is_accepted() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-channels.yaml"),
    )
    .await;

    assert!(
        post(&proc, &fake, "pva").await.is_success(),
        "a channel on `allowed_channel_ids` must be served — this is what \
         makes Copilot Studio reachable at all"
    );
    let dispatch = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch"
    });
    assert_eq!(dispatch["tenant"], TENANT, "got: {dispatch}");
}

/// Teams keeps working from the same allowlist — no regression for the
/// channel that works today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_teams_channel_still_works() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-channels.yaml"),
    )
    .await;
    assert!(post(&proc, &fake, "msteams").await.is_success());
}

/// The security half: a channel NOT on the list is still refused, so a
/// valid token for this bot arriving over some other channel cannot
/// inject an Entra-shaped principal. Widening the gate must not remove
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undeclared_channel_is_still_refused() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-channels.yaml"),
    )
    .await;
    assert_eq!(
        post(&proc, &fake, "directline").await,
        401,
        "an undeclared channel must still be refused"
    );
    assert_eq!(
        post(&proc, &fake, "").await,
        401,
        "an empty channelId must be refused, not treated as a wildcard"
    );
}

/// A deployment that does not declare the field keeps today's behaviour
/// exactly: Teams only. The widening is opt-in, so no existing manifest
/// changes meaning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_is_still_teams_only() {
    let fake = FakeBotFramework::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&fake, "manifest-msteams-azure.yaml"),
    )
    .await;
    assert!(post(&proc, &fake, "msteams").await.is_success());
    assert_eq!(
        post(&proc, &fake, "pva").await,
        401,
        "without an explicit allowlist the adapter stays Teams-only"
    );
}

/// #250: the unresolved Direct Line question, made moot.
///
/// On Direct Line-family channels (WebChat, and Copilot Studio's canvas)
/// Microsoft's connector may mint a valid bot token for an ANONYMOUS
/// user whose `from.id` the client chooses. Nobody in this repo can
/// settle that without an Azure probe — but under `identity.kind: azure`
/// the entire principal is read from those very fields, so if it is
/// true, listing such a channel turns sender-id-into-principal with a
/// caller-chosen id.
///
/// Rather than wait for the probe, refuse the combination at boot. The
/// operator gets a named error instead of a silent trust assumption, and
/// the probe's outcome stops being load-bearing for safety: `azure`
/// simply is not available on a channel whose ids are client-chosen.
/// Serving those channels needs an identity mode that does not read the
/// principal off the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_identity_refuses_to_boot_on_a_client_id_channel() {
    let bin = locate_triton_binary();
    let mut child = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env(
            "TRITON_MANIFEST_PATH",
            manifest_path("manifest-msteams-directline.yaml"),
        )
        .env(
            "TRITON_MSTEAMS_OPENID_URL",
            "https://login.botframework.com",
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!(
                    "azure identity on a client-id channel MUST refuse boot, \
                     but the binary kept running"
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(!status.success(), "must exit non-zero; got {status:?}");
    // The reason is on stdout (structured JSON logs) or stderr,
    // depending on how the boot error surfaces; read both.
    use std::io::Read;
    let mut out = String::new();
    let mut err = String::new();
    let _ = child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out);
    let _ = child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err);
    let combined = format!("{out}{err}");
    assert!(
        combined.contains("directline") && combined.contains("client-chosen"),
        "the refusal must name the offending channel AND say why, or an \
         operator cannot act on it; got: {combined}"
    );
}

/// A security gate whose closed set depends on Microsoft's casing is
/// the very assumption this branch set out to remove. `"DirectLine"`
/// must be refused exactly like `"directline"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_id_channel_refusal_is_case_insensitive() {
    assert_boot_refused(
        "manifest-msteams-directline-cased.yaml",
        &["directline", "client-chosen"],
    );
}

/// An explicitly empty `allowed_channel_ids` silently disables the
/// adapter — every Activity refused, no error anywhere. Empty
/// `allowed_tenants` is already a named boot error; this is the same
/// class of fail-silent misconfiguration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_channel_allowlist_refuses_to_boot() {
    assert_boot_refused("manifest-msteams-nochannels.yaml", &["allowed_channel_ids"]);
}

/// #250, the original defect — closed by making the configuration
/// unrepresentable rather than by trying to verify the unverifiable.
///
/// Under `identity.kind: azure` the tenant is read from
/// `channelData.tenant.id`: unsigned body metadata. With ONE allowed
/// tenant that is harmless — the only value that passes is the only
/// value it could have been, so the check is equivalent to pinning.
/// With two or more, the body field becomes a privilege SELECTOR: a
/// caller who can present an Activity chooses which tenant's data the
/// downstream Escurel token is scoped to, and nothing in the Bot
/// Framework transport can contradict them (the connector token carries
/// no `tid`; see doc/realizations.md §7).
///
/// Nobody can make that assertion trustworthy, so the fix is to refuse
/// the configuration. Multi-tenant needs an identity mode that does not
/// read the tenant off the body — `upstream`, where a resolver decides
/// it — or one bot registration per tenant, which puts the tenant in the
/// credential. Both remain available; the unsafe middle does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_identity_refuses_more_than_one_tenant() {
    assert_boot_refused(
        "manifest-msteams-multitenant.yaml",
        &["allowed_tenants", "upstream"],
    );
}

/// Spawn with `manifest` and assert the binary exits non-zero with a
/// message containing every fragment in `must_contain`.
fn assert_boot_refused(manifest: &str, must_contain: &[&str]) {
    let bin = locate_triton_binary();
    let mut child = std::process::Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path(manifest))
        .env(
            "TRITON_MSTEAMS_OPENID_URL",
            "https://login.botframework.com",
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!("{manifest} MUST refuse boot, but the binary kept running");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(!status.success(), "must exit non-zero; got {status:?}");
    use std::io::Read;
    let mut out = String::new();
    let mut err = String::new();
    let _ = child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out);
    let _ = child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err);
    let combined = format!("{out}{err}").to_lowercase();
    for frag in must_contain {
        assert!(
            combined.contains(&frag.to_lowercase()),
            "the refusal must mention `{frag}`; got: {combined}"
        );
    }
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let cand = here.join("target/debug/triton");
        if cand.exists() {
            return cand;
        }
        here.pop();
    }
    panic!("triton binary not found");
}

fn wait_for_audit(proc: &TritonProcess, deadline: Duration, m: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if m(&v) {
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
