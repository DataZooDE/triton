//! The spec-conformant A2A face: JSON-RPC + Agent Card (#214).
//!
//! Triton's original A2A route speaks a Triton-shaped message carrying
//! `{tool, args}`. Agent platforms (Gemini Enterprise, Microsoft Copilot
//! Studio) know only the published protocol: they fetch an Agent Card and
//! then POST JSON-RPC `message/send` with plain text. These tests pin
//! that second face, and — just as importantly — that turning it on does
//! not disturb the first.
//!
//! No mocks: a real `TestIssuer`, real signed tokens, a real socket.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};
use triton_tests::TestIssuer;

const AUD: &str = "a2a-spec-audience";
const PUBLIC_URL: &str = "https://agent.example.test";

/// Answers with the A2UI-ish shape a real Triton tool returns, so the
/// text-extraction path is exercised rather than a bare string.
struct AssistantTool;

#[async_trait]
impl Tool for AssistantTool {
    fn name(&self) -> &'static str {
        "assistant"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
        if msg == "boom" {
            return Err(TritonError::Validation("tool refused".into()));
        }
        Ok(json!({ "surface": { "text": format!("you said: {msg}") } }))
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
    reg.register(Arc::new(AssistantTool));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), "test".to_string()));
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// An issuer + a spec-A2A-enabled host.
async fn spec_host() -> (TestIssuer, String) {
    let iss = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(iss.issuer_url(), AUD, None).spec_a2a(
        "DataZoo Agent",
        "Answers questions.",
        PUBLIC_URL,
        "assistant",
    ))
    .await;
    (iss, base)
}

async fn rpc(base: &str, token: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a"))
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
        "params": {
            "message": {
                "role": "user",
                "messageId": "m-1",
                "parts": [{ "kind": "text", "text": text }]
            }
        }
    })
}

/// The card is fetchable WITHOUT a credential — discovery happens before
/// a caller has one, and the card's job is to say which one to bring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_card_is_public_and_describes_the_endpoint() {
    let (iss, base) = spec_host().await;

    for path in ["/.well-known/agent-card.json", "/.well-known/agent.json"] {
        let resp = reqwest::get(format!("{base}{path}")).await.expect("GET");
        assert_eq!(resp.status(), 200, "{path} must be public");
        let card: Value = resp.json().await.expect("json");

        assert_eq!(card["name"], "DataZoo Agent");
        assert_eq!(card["protocolVersion"], "0.3.0");
        assert_eq!(card["preferredTransport"], "JSONRPC");
        // The card must advertise where CALLERS reach the agent, which is
        // the public origin, never the socket it happens to be bound to.
        assert_eq!(card["url"], format!("{PUBLIC_URL}/a2a"));
        assert!(!base.contains("example.test"));

        // Skills come from the live registry, so the card cannot drift
        // from what the agent can actually do.
        let skills = card["skills"].as_array().expect("skills");
        assert!(skills.iter().any(|s| s["id"] == "assistant"), "{card}");

        // And it names the credential to bring.
        assert_eq!(card["securitySchemes"]["bearer"]["scheme"], "bearer");
        let desc = card["securitySchemes"]["bearer"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains(&iss.issuer_url()), "{desc}");
        assert!(desc.contains(AUD), "{desc}");
    }
}

/// A spec caller sends prose and gets prose back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_send_dispatches_text_and_returns_an_agent_message() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    let (status, body) = rpc(&base, &token, send("hello there")).await;
    assert_eq!(status, 200);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let result = &body["result"];
    assert_eq!(result["kind"], "message");
    assert_eq!(result["role"], "agent");
    assert_eq!(result["parts"][0]["kind"], "text");
    assert_eq!(result["parts"][0]["text"], "you said: hello there");
    assert!(result["messageId"].is_string());
}

/// `contextId` is echoed so a platform can thread a conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supplied_context_id_is_echoed_back() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));
    let mut req = send("hi");
    req["params"]["message"]["contextId"] = json!("ctx-42");

    let (_, body) = rpc(&base, &token, req).await;
    assert_eq!(body["result"]["contextId"], "ctx-42");
}

/// The task recorded by message/send is retrievable by tasks/get, and an
/// unknown id gets A2A's TaskNotFound rather than a generic failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tasks_get_finds_a_task_and_reports_unknown_ids() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    let (_, sent) = rpc(&base, &token, send("track me")).await;
    let task_id = sent["result"]["taskId"]
        .as_str()
        .expect("taskId")
        .to_string();

    let (_, got) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"2.0","id":2,"method":"tasks/get","params":{"id":task_id}}),
    )
    .await;
    assert_eq!(got["result"]["kind"], "task");
    assert_eq!(got["result"]["status"]["state"], "completed");

    let (_, missing) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"2.0","id":3,"method":"tasks/get","params":{"id":"nope"}}),
    )
    .await;
    assert_eq!(missing["error"]["code"], -32001, "A2A TaskNotFound");
}

/// Protocol-level errors are JSON-RPC error objects, not HTTP failures:
/// the transport succeeded, the call did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_errors_are_jsonrpc_errors_with_the_right_codes() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    // Unknown method → -32601, and the message names what IS supported.
    let (status, body) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"2.0","id":9,"method":"message/stream","params":{}}),
    )
    .await;
    assert_eq!(status, 200, "a JSON-RPC error is still HTTP 200");
    assert_eq!(body["error"]["code"], -32601);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("message/send"),
        "the error should name the supported methods: {body}"
    );

    // Missing text part → -32602.
    let (_, body) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"2.0","id":10,"method":"message/send","params":{"message":{"parts":[]}}}),
    )
    .await;
    assert_eq!(body["error"]["code"], -32602);

    // Wrong protocol version → -32600.
    let (_, body) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"1.0","id":11,"method":"message/send","params":{}}),
    )
    .await;
    assert_eq!(body["error"]["code"], -32600);

    // A tool that fails → -32603, not a panic or a 500.
    let (_, body) = rpc(&base, &token, send("boom")).await;
    assert_eq!(body["error"]["code"], -32603);
}

/// Auth is the SAME boundary as every other surface, and an
/// unauthenticated caller gets 401 — the status its client library
/// already understands — not a 200 carrying an error object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_jsonrpc_route_is_behind_the_identity_boundary() {
    let (_iss, base) = spec_host().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a"))
        .json(&send("let me in"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401, "no bearer must be refused");

    let (status, _) = rpc(&base, "dev-token", send("let me in")).await;
    assert_eq!(status, 401, "OIDC configured closes the dev-token path");
}

/// Turning the spec face on must not disturb the Triton-shaped route
/// that existing callers use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_triton_shaped_route_still_works_alongside_it() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a/message:send"))
        .bearer_auth(&token)
        .json(&json!({
            "parts": [{ "data": { "tool": "assistant", "args": { "message": "legacy" } } }]
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert!(
        body["metadata"]["trace_id"].is_string(),
        "the Triton-shaped response shape is unchanged: {body}"
    );
}

/// Opting OUT leaves the surface exactly as it was: no card, no
/// JSON-RPC route, and the Triton-shaped route untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_config_neither_the_card_nor_the_jsonrpc_route_exists() {
    let iss = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(iss.issuer_url(), AUD, None)).await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    for path in ["/.well-known/agent-card.json", "/.well-known/agent.json"] {
        let resp = reqwest::get(format!("{base}{path}")).await.expect("GET");
        assert_eq!(
            resp.status(),
            404,
            "{path} must not exist when unconfigured"
        );
    }

    // 404, not 405: without the facade nothing is mounted at the A2A
    // base path at all, so it is not a route rather than a route
    // rejecting the method.
    let (status, _) = rpc(&base, &token, send("hi")).await;
    assert_eq!(status, 404, "POST /a2a is not a route when unconfigured");
}
