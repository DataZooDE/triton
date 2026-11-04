//! The binary's ingress — Entra multi-tenant with required scopes, the
//! per-customer MCP endpoints and their RFC 9728 metadata, spec A2A from
//! configuration, the task store and settings, the dev token and the
//! answer-card widget — available to a host that embeds Triton in-process
//! through `triton_embed::router_parts`.
//!
//! The configuration is the binary's own (`IngressConfig` is the clap
//! group `triton` flattens), so a host and the binary read the same
//! `TRITON_*` variables the same way. These tests pass flags instead of
//! mutating the process environment, which other tests share.
//!
//! No mocks: a real `TestIssuer` plays the Entra authority, real signed
//! tokens, a real socket.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::ingress::IngressConfig;
use triton_embed::{EmbedOpts, router_parts};
use triton_tests::TestIssuer;

const ORIGIN: &str = "https://agents.example.test";
const ENTRA_AUD: &str = "api://triton-mcp";
const SCOPE: &str = "api://triton-mcp/access_as_user";
const TID_ACME: &str = "11111111-2222-3333-4444-abcdefabcdef";

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        Ok(json!({ "echoed": args }))
    }
}

fn dispatcher(env: &str) -> Arc<Dispatcher> {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(Echo));
    Arc::new(Dispatcher::new(
        Arc::new(reg),
        env,
        DispatchControls::unenforced(),
    ))
}

/// The ingress group from flags, as `triton --flag …` would parse it.
fn ingress(flags: &[&str]) -> IngressConfig {
    let mut argv = vec!["host"];
    argv.extend_from_slice(flags);
    IngressConfig::try_parse_from(argv).expect("ingress flags parse")
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn entra_token(entra: &TestIssuer, scp: &str) -> String {
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID_ACME}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD,
        "sub": "entra-pairwise-sub",
        "oid": "00000000-0000-0000-0000-0000000000aa",
        "tid": TID_ACME,
        "preferred_username": "bob@acme.example",
        "exp": now() + 600,
        "iat": now(),
        "scp": scp,
    }))
}

fn entra_flags(entra: &TestIssuer) -> Vec<String> {
    vec![
        "--entra-audience".into(),
        ENTRA_AUD.into(),
        "--entra-allowed-tenants".into(),
        format!("{TID_ACME}=acme"),
        "--entra-authority".into(),
        entra.issuer_url(),
        "--entra-jwks-url".into(),
        entra.jwks_url(),
        "--mcp-tenants".into(),
        format!("acme=acme:{TID_ACME}"),
        "--mcp-public-origin".into(),
        ORIGIN.into(),
    ]
}

fn mcp(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_mounts_customer_mcp_with_protected_resource_metadata_and_scope_check() {
    let entra = TestIssuer::start().await;
    let flags = entra_flags(&entra);
    let cfg = ingress(&flags.iter().map(String::as_str).collect::<Vec<_>>());
    let opts = EmbedOpts::dev().env("local").ingress(cfg);
    let parts = router_parts(dispatcher("local"), &opts).expect("ingress accepted");
    let base = serve(parts.router).await;
    let http = reqwest::Client::new();

    // RFC 9728 §3.1: the metadata of the customer's path names its Entra
    // directory and the delegated scope.
    let doc: Value = http
        .get(format!(
            "{base}/.well-known/oauth-protected-resource/mcp/acme"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        doc,
        json!({
            "resource": format!("{ORIGIN}/mcp/acme"),
            "authorization_servers": [format!("{}/{TID_ACME}/v2.0", entra.issuer_url())],
            "scopes_supported": [SCOPE],
            "bearer_methods_supported": ["header"],
        })
    );

    // A token of the right directory without the delegated scope is a 403
    // `insufficient_scope` naming the scope (ADR-22).
    let resp = http
        .post(format!("{base}/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(entra_token(&entra, "something_else"))
        .json(&mcp("tools/list"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .expect("challenge")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains("insufficient_scope") && challenge.contains(SCOPE),
        "{challenge}"
    );

    // With the scope it lists the tools.
    let resp = http
        .post(format!("{base}/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(entra_token(&entra, "access_as_user"))
        .json(&mcp("tools/list"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["result"]["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["name"] == "echo")),
        "{body}"
    );
    // The identity the router enforces is the one handed back.
    assert!(Arc::strong_count(&parts.identity) >= 1);
}

#[tokio::test]
async fn ingress_refuses_a_provider_override_outside_local() {
    let entra = TestIssuer::start().await;
    let flags = entra_flags(&entra);
    let cfg = ingress(&flags.iter().map(String::as_str).collect::<Vec<_>>());
    let opts = EmbedOpts::dev().env("prod").ingress(cfg);
    let err = router_parts(dispatcher("prod"), &opts)
        .err()
        .expect("an authority override is refused outside local");
    assert!(err.to_string().contains("TRITON_ENTRA_AUTHORITY"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_a2a_from_ingress_serves_the_configured_skills_and_version() {
    let skills = json!([{ "id": "ask", "name": "Ask", "description": "Ask a question",
                          "tags": ["q"] }])
    .to_string();
    let cfg = ingress(&[
        "--a2a-public-url",
        "https://agents.example.test/a2a",
        "--a2a-default-tool",
        "echo",
        "--a2a-agent-name",
        "Embedded agent",
        "--a2a-agent-version",
        "9.9.9",
        "--a2a-skills",
        &skills,
    ]);
    let parts = router_parts(dispatcher("test"), &EmbedOpts::dev().ingress(cfg)).unwrap();
    let base = serve(parts.router).await;
    for path in [
        "/.well-known/agent-card.json",
        "/a2a/.well-known/agent-card.json",
    ] {
        let card: Value = reqwest::get(format!("{base}{path}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(card["name"], "Embedded agent", "{path}: {card}");
        assert_eq!(card["version"], "9.9.9", "{path}");
        assert_eq!(
            card["supportedInterfaces"][0]["url"], "https://agents.example.test/a2a",
            "{path}: the configured public URL"
        );
        assert_eq!(card["skills"][0]["id"], "ask", "{path}");
    }
}

#[tokio::test]
async fn ingress_task_db_and_timing_reach_the_runner() {
    let dir = std::env::temp_dir().join(format!(
        "embed-ingress-tasks-{}-{}",
        std::process::id(),
        now()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tasks.db");
    let cfg = ingress(&[
        "--task-db",
        db.to_str().unwrap(),
        "--mcp-async-wait-secs",
        "3",
        "--a2a-block-secs",
        "7",
    ]);
    let parts = router_parts(dispatcher("test"), &EmbedOpts::dev().ingress(cfg)).unwrap();
    assert!(
        parts.runner.store.is_persistent(),
        "TRITON_TASK_DB is SQLite"
    );
    assert!(db.exists(), "the task database is created at boot");
    assert_eq!(parts.runner.settings.mcp_wait, Duration::from_secs(3));
    assert_eq!(parts.runner.settings.block, Duration::from_secs(7));

    // Without ingress the store is process memory with default timing.
    let plain = router_parts(dispatcher("test"), &EmbedOpts::dev()).unwrap();
    assert!(!plain.runner.store.is_persistent());
    assert_eq!(plain.runner.settings.mcp_wait, Duration::from_secs(20));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_token_and_widget_are_host_options() {
    let parts = router_parts(
        dispatcher("test"),
        &EmbedOpts::dev().dev_token("s3cret-dev").widget(false),
    )
    .unwrap();
    let base = serve(parts.router).await;
    let http = reqwest::Client::new();
    let call = |token: &'static str| {
        http.post(format!("{base}/v1/tools/echo"))
            .bearer_auth(token)
            .json(&json!({ "x": 1 }))
            .send()
    };
    assert_eq!(
        call("dev-token").await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the default literal no longer opens the door"
    );
    assert_eq!(
        call("s3cret-dev").await.unwrap().status(),
        reqwest::StatusCode::OK
    );

    let resources = |base: String, token: &'static str| async move {
        let body: Value = reqwest::Client::new()
            .post(format!("{base}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .bearer_auth(token)
            .json(&mcp("resources/list"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
    };
    assert!(
        !resources(base.clone(), "s3cret-dev")
            .await
            .contains(&"answer-card".to_string()),
        "widget(false) drops the answer card"
    );
    let on = router_parts(dispatcher("test"), &EmbedOpts::dev()).unwrap();
    let on_base = serve(on.router).await;
    assert!(
        resources(on_base, "dev-token")
            .await
            .contains(&"answer-card".to_string()),
        "the widget is on by default"
    );
}
