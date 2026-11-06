//! A Microsoft Teams adapter inside a host that embeds Triton:
//! `triton_embed::chat::msteams` builds it from the manifest with the
//! binary's NFR-S-4 checks and the same task runner as A2A and MCP, and
//! `triton_embed::surfaces::registry` gives the dispatcher the Teams
//! surface the agent is told about.
//!
//! No mocks: `FakeBotFramework` signs the activity and receives the reply,
//! `FakeAsyncAgent` is a real upstream over HTTP that goes asynchronous, the
//! task store is a real SQLite file.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;
use triton_core::dispatcher::DispatchControls;
use triton_core::{Dispatcher, ToolRegistry};
use triton_embed::chat::LiteralResolver;
use triton_embed::chat::{MsTeamsConfig, MsTeamsError};
use triton_embed::{EmbedOpts, router_parts};
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
use triton_tests::chat_courier_fixture::FakeBotFramework;

const ANSWER: &str = "4 drafts ready for review.";

fn manifest() -> triton_manifest::Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("manifest-msteams-async-test.yaml");
    triton_manifest::Manifest::load(&path).expect("fixture manifest")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn dispatcher(agent: &FakeAsyncAgent, m: &triton_manifest::Manifest) -> Arc<Dispatcher> {
    let upstream = triton_upstream::StaticUpstream::from_spec(
        &format!("answer={}", agent.host_port()),
        "dev-token".to_string(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(30),
        5,
        Duration::from_secs(30),
    );
    Arc::new(
        Dispatcher::new(
            Arc::new(ToolRegistry::new()),
            "local".to_string(),
            DispatchControls::unenforced(),
        )
        .with_surfaces(triton_embed::surfaces::registry(Some(m)).expect("registry"))
        .with_upstream(Arc::new(upstream)),
    )
}

/// The Teams settings from flags, as `triton --flag …` would parse them.
fn teams(flags: &[&str]) -> MsTeamsConfig {
    let mut argv = vec!["host"];
    argv.extend_from_slice(flags);
    MsTeamsConfig::try_parse_from(argv).expect("teams flags parse")
}

fn local_opts(fake: &FakeBotFramework) -> MsTeamsConfig {
    let (openid, token) = (fake.openid_url(), fake.token_url());
    teams(&[
        "--msteams-openid-url",
        &openid,
        "--msteams-token-url",
        &token,
        "--msteams-extra-service-url-hosts",
        "127.0.0.1",
        "--msteams-async",
        "1",
    ])
}

fn fixture(name: &str) -> triton_manifest::Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    triton_manifest::Manifest::load(&path).expect("fixture manifest")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_embedded_teams_adapter_answers_a_long_running_turn_through_the_shared_task_store() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(800),
        ..AsyncPlan::default()
    })
    .await;
    let m = manifest();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "embed-msteams-{}-{}",
        std::process::id(),
        now()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tasks.db");
    let cfg = <triton_embed::ingress::IngressConfig as clap::Parser>::try_parse_from([
        "host",
        "--task-db",
        db.to_str().unwrap(),
    ])
    .unwrap();

    let dispatcher = dispatcher(&agent, &m);
    let parts = router_parts(
        dispatcher.clone(),
        &EmbedOpts::dev().env("local").ingress(cfg),
    )
    .expect("parts");
    let (teams, adapter) = triton_embed::chat::msteams(
        &m,
        "msteams",
        "local",
        dispatcher,
        parts.runner.clone(),
        &local_opts(&fake),
        &LiteralResolver,
        &mut false,
    )
    .await
    .expect("the Teams adapter builds");
    assert!(!adapter.canonical_path());
    let app = parts.router.merge(teams);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com", "aud": "triton-msteams-test-appid",
        "exp": now() + 600, "iat": now() - 5, "serviceurl": fake.service_url(),
    }));
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&json!({
            "type": "message", "id": "msg-1",
            "timestamp": "2026-10-31T10:00:00.0000000Z",
            "serviceUrl": "https://placeholder.example/", "channelId": "msteams",
            "from": { "id": "29:1abc", "name": "Alice" },
            "conversation": { "id": "a:conv-1" },
            "recipient": { "id": "28:bot-1", "name": "MyBot" },
            "text": "Propose an owner for every Agent without one.",
            "textFormat": "plain"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());

    // The answer reaches Teams (as a new message or an update of the
    // working card) once the upstream task finishes.
    let start = Instant::now();
    loop {
        let delivered = fake
            .updates()
            .iter()
            .any(|u| u.body.to_string().contains(ANSWER))
            || fake
                .captured()
                .iter()
                .any(|c| c.body.to_string().contains(ANSWER));
        if delivered {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "no answer delivered; captured {:?}",
            fake.captured()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The turn was a task in the host's store, and the agent saw the Teams
    // surface the registry describes.
    let req = agent.requests().into_iter().next().expect("agent called");
    assert_eq!(req.body["_triton"]["surface"]["channel"], "msteams");
    let stored = parts.runner.store.list_running("messenger:msteams");
    assert!(stored.is_empty(), "the task finished: {stored:?}");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let records: Vec<String> = conn
        .prepare("SELECT record FROM triton_tasks")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        records.iter().any(|r| r.contains("messenger:msteams")
            && r.contains("completed")
            && r.contains(ANSWER)),
        "the Teams turn is a completed task in TRITON_TASK_DB: {records:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_embedded_teams_adapter_refuses_endpoint_overrides_outside_local() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let m = manifest();
    let dispatcher = dispatcher(&agent, &m);
    let parts = router_parts(dispatcher.clone(), &EmbedOpts::dev()).unwrap();
    let err = triton_embed::chat::msteams(
        &m,
        "msteams",
        "prod",
        dispatcher,
        parts.runner,
        &local_opts(&fake),
        &LiteralResolver,
        &mut false,
    )
    .await
    .err()
    .expect("refused outside local");
    assert!(
        matches!(&err, MsTeamsError::Egress(m) if m.contains("TRITON_MSTEAMS_OPENID_URL")),
        "{err}"
    );
}

/// The Teams settings are the binary's clap group: defaults, the `true`/`1`
/// switch, the host list, and an empty override kept as a value — which is
/// refused outside `local` exactly as the binary refuses it.
#[tokio::test]
async fn teams_settings_parse_like_the_binary_and_an_empty_override_is_refused() {
    let d = teams(&[]);
    assert_eq!(
        d.msteams_openid_url,
        "https://login.botframework.com/v1/.well-known/openidconfiguration"
    );
    assert_eq!(d.msteams_token_url, None);
    assert!(d.msteams_extra_service_url_hosts.0.is_empty());
    assert!(!d.msteams_async);
    assert!(teams(&["--msteams-async", "TRUE"]).msteams_async);
    assert!(!teams(&["--msteams-async", "yes"]).msteams_async);
    assert_eq!(
        teams(&["--msteams-extra-service-url-hosts", " a, ,b "])
            .msteams_extra_service_url_hosts
            .0,
        vec!["a".to_string(), "b".to_string()]
    );

    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let m = manifest();
    let dispatcher = dispatcher(&agent, &m);
    let parts = router_parts(dispatcher.clone(), &EmbedOpts::dev()).unwrap();
    let empty = teams(&["--msteams-token-url", ""]);
    assert_eq!(empty.msteams_token_url.as_deref(), Some(""));
    let err = triton_embed::chat::msteams(
        &m,
        "msteams",
        "prod",
        dispatcher,
        parts.runner,
        &empty,
        &LiteralResolver,
        &mut false,
    )
    .await
    .err()
    .expect("an empty token URL is still an override");
    assert!(
        matches!(&err, MsTeamsError::Egress(m) if m.contains("TRITON_MSTEAMS_TOKEN_URL")),
        "{err}"
    );
    drop(fake);
}

/// triton#247: the second adapter claiming `/api/messages` is refused by
/// the library itself, which records the first claim.
#[tokio::test]
async fn a_second_canonical_path_claimant_is_refused() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let m = fixture("manifest-msteams-canonical.yaml");
    let dispatcher = dispatcher(&agent, &m);
    let parts = router_parts(dispatcher.clone(), &EmbedOpts::dev()).unwrap();
    let mut claimed = false;
    let (_, first) = triton_embed::chat::msteams(
        &m,
        "msteams",
        "local",
        dispatcher.clone(),
        parts.runner.clone(),
        &local_opts(&fake),
        &LiteralResolver,
        &mut claimed,
    )
    .await
    .expect("the first claimant builds");
    assert!(first.canonical_path() && claimed, "the claim is recorded");
    let err = triton_embed::chat::msteams(
        &m,
        "msteams",
        "local",
        dispatcher,
        parts.runner,
        &local_opts(&fake),
        &LiteralResolver,
        &mut claimed,
    )
    .await
    .err()
    .expect("the second claimant is refused");
    assert!(matches!(err, MsTeamsError::CanonicalPathTaken(_)), "{err}");
}

/// Only a webhook Teams adapter has a router to mount.
#[tokio::test]
async fn a_non_webhook_teams_adapter_is_refused() {
    let fake = FakeBotFramework::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let mut m = manifest();
    m.adapters.get_mut("msteams").unwrap().inbound.kind = triton_manifest::InboundKind::LongPoll;
    let dispatcher = dispatcher(&agent, &m);
    let parts = router_parts(dispatcher.clone(), &EmbedOpts::dev()).unwrap();
    let err = triton_embed::chat::msteams(
        &m,
        "msteams",
        "local",
        dispatcher,
        parts.runner,
        &local_opts(&fake),
        &LiteralResolver,
        &mut false,
    )
    .await
    .err()
    .expect("refused");
    assert!(matches!(err, MsTeamsError::NotWebhook(_)), "{err}");
}
