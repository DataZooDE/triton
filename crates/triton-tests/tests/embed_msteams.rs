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

use serde_json::json;
use triton_core::dispatcher::DispatchControls;
use triton_core::{Dispatcher, ToolRegistry};
use triton_embed::chat::{MsTeamsError, MsTeamsOpts};
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

fn local_opts(fake: &FakeBotFramework) -> MsTeamsOpts {
    MsTeamsOpts {
        openid_url: fake.openid_url(),
        token_url: Some(fake.token_url()),
        extra_service_url_hosts: vec!["127.0.0.1".to_string()],
        async_reply: true,
        canonical_path_taken: false,
    }
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
    )
    .await
    .err()
    .expect("refused outside local");
    assert!(
        matches!(&err, MsTeamsError::Egress(m) if m.contains("TRITON_MSTEAMS_OPENID_URL")),
        "{err}"
    );
}
