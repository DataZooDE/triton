//! Per-agent spec-A2A faces (`/a2a/<slug>`): one Agent Card + JSON-RPC
//! endpoint per agent, so Gemini Enterprise can register one card per
//! roster agent of a single host.
//!
//! No mocks: a real `TestIssuer`, real signed tokens, a real socket.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};
use triton_tests::TestIssuer;

const AUD: &str = "a2a-agents-audience";
const PUBLIC_URL: &str = "https://agent.example.test";

/// Answers with its OWN name, so a test can assert which tool a
/// per-agent endpoint actually reached.
struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
        Ok(json!({ "surface": { "text": format!("{} heard: {msg}", self.0) } }))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn claims(iss: &str) -> Value {
    json!({ "iss": iss, "aud": AUD, "sub": "alice", "exp": now() + 600, "iat": now() - 5 })
}

async fn serve(opts: EmbedOpts) -> String {
    let mut reg = ToolRegistry::new();
    for name in ["assistant", "sales_analyst", "supply_planner"] {
        reg.register(Arc::new(NamedTool(name)));
    }
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(reg),
        "test".to_string(),
        DispatchControls::unenforced(),
    ));
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn agents_opts(iss: &TestIssuer) -> EmbedOpts {
    EmbedOpts::dev()
        .oidc(iss.issuer_url(), AUD, None)
        .spec_a2a(
            "DataZoo Agent",
            "Answers questions.",
            PUBLIC_URL,
            "assistant",
        )
        .spec_a2a_agent("Sales Analyst", "Sells.", "sales", "sales_analyst")
        .unwrap()
        .spec_a2a_agent(
            "Supply Planner",
            "Plans.",
            "supply-planner",
            "supply_planner",
        )
        .unwrap()
}

async fn agents_host() -> (TestIssuer, String) {
    let iss = TestIssuer::start().await;
    let base = serve(agents_opts(&iss)).await;
    (iss, base)
}

async fn rpc_at(base: &str, path: &str, token: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST");
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn send(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": { "message": {
            "role": "user", "messageId": "m-1",
            "parts": [{ "kind": "text", "text": text }]
        } }
    })
}

const FILENAMES: [&str; 5] = [
    "agent-card.json",
    "agent.json",
    "agentcard.json",
    "agentCard.json",
    "agent_card.json",
];

/// Each agent's card advertises ITS OWN endpoint and name, at every
/// filename spelling, publicly — and the primary card is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_agent_card_advertises_its_own_url_and_name() {
    let (iss, base) = agents_host().await;
    for (slug, name, desc) in [
        ("sales", "Sales Analyst", "Sells."),
        ("supply-planner", "Supply Planner", "Plans."),
    ] {
        for f in FILENAMES {
            let path = format!("/a2a/{slug}/.well-known/{f}");
            let resp = reqwest::get(format!("{base}{path}")).await.expect("GET");
            assert_eq!(resp.status(), 200, "{path} must be public");
            let card: Value = resp.json().await.expect("json");
            assert_eq!(card["name"], name, "{path}: {card}");
            assert_eq!(card["description"], desc, "{path}: {card}");
            assert_eq!(card["url"], format!("{PUBLIC_URL}/a2a/{slug}"), "{path}");
            assert_eq!(card["protocolVersion"], "0.3.0");
            assert_eq!(card["version"], env!("CARGO_PKG_VERSION"));
            // Same credential advertisement as the root card.
            let d = card["securitySchemes"]["bearer"]["description"]
                .as_str()
                .unwrap();
            assert!(d.contains(&iss.issuer_url()) && d.contains(AUD), "{d}");
        }
    }
    // The primary card (root + endpoint-relative) is still the primary.
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
        assert_eq!(card["name"], "DataZoo Agent");
        assert_eq!(card["url"], format!("{PUBLIC_URL}/a2a"));
    }
}

/// Adding per-agent faces must leave the root card byte-for-byte as it
/// was without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_card_is_byte_identical_with_agents_configured() {
    let iss = TestIssuer::start().await;
    let plain = serve(EmbedOpts::dev().oidc(iss.issuer_url(), AUD, None).spec_a2a(
        "DataZoo Agent",
        "Answers questions.",
        PUBLIC_URL,
        "assistant",
    ))
    .await;
    let with_agents = serve(agents_opts(&iss)).await;
    for path in [
        "/.well-known/agent-card.json",
        "/a2a/.well-known/agent.json",
    ] {
        let a = reqwest::get(format!("{plain}{path}"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let b = reqwest::get(format!("{with_agents}{path}"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(a, b, "{path} changed when agents were added");
    }
}

/// `message/send` on `/a2a/<slug>` reaches THAT agent's tool, and the
/// root `/a2a` still reaches the default tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_on_an_agent_endpoint_dispatches_to_that_tool() {
    let (iss, base) = agents_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));
    for (path, tool) in [
        ("/a2a/sales", "sales_analyst"),
        ("/a2a/supply-planner", "supply_planner"),
        ("/a2a", "assistant"),
    ] {
        let (status, body) = rpc_at(&base, path, &token, send("hello")).await;
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!(
            body["result"]["parts"][0]["text"],
            format!("{tool} heard: hello"),
            "{path}: {body}"
        );
    }
    // The Triton-shaped route is undisturbed.
    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a/message:send"))
        .bearer_auth(&token)
        .json(&json!({ "parts": [{ "data": { "tool": "assistant", "args": { "message": "legacy" } } }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// The per-agent endpoint sits behind the same identity boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_endpoint_is_behind_the_identity_boundary() {
    let (_iss, base) = agents_host().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a/sales"))
        .json(&send("let me in"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let (status, _) = rpc_at(&base, "/a2a/sales", "dev-token", send("x")).await;
    assert_eq!(status, 401, "OIDC configured closes the dev-token path");
}

/// An unconfigured slug is not a route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_slug_is_a_404() {
    let (iss, base) = agents_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));
    let (status, _) = rpc_at(&base, "/a2a/nobody", &token, send("hi")).await;
    assert_eq!(status, 404);
    let resp = reqwest::get(format!("{base}/a2a/nobody/.well-known/agent-card.json"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Invalid, reserved and duplicate slugs are refused at configuration.
#[test]
fn invalid_slugs_are_rejected() {
    for bad in [
        "",
        "Sales",
        "message:send",
        ".well-known",
        "a/b",
        "a b",
        "-x",
        "x-",
        "message",
        "tasks",
        "well-known",
        "sales_analyst",
    ] {
        assert!(
            EmbedOpts::dev().spec_a2a_agent("n", "d", bad, "t").is_err(),
            "`{bad}` must be rejected"
        );
    }
    let dup = EmbedOpts::dev()
        .spec_a2a_agent("a", "d", "sales", "t")
        .unwrap()
        .spec_a2a_agent("b", "d", "sales", "t");
    assert!(dup.is_err(), "a duplicate slug must be rejected");
}

/// Without the primary `spec_a2a` there is no public URL, so the agents
/// are ignored rather than mounted with a broken card.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_without_the_primary_config_are_ignored() {
    let iss = TestIssuer::start().await;
    let base = serve(
        EmbedOpts::dev()
            .oidc(iss.issuer_url(), AUD, None)
            .spec_a2a_agent("Sales Analyst", "Sells.", "sales", "sales_analyst")
            .unwrap(),
    )
    .await;
    let resp = reqwest::get(format!("{base}/a2a/sales/.well-known/agent-card.json"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
