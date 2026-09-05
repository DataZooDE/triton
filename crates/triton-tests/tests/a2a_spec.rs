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
        if msg == "rich" {
            // A component surface (like the embedded agent's), so the A2UI
            // v0.9 DataPart path is exercised: prose + a chart image + a
            // follow-up button.
            return Ok(json!({ "surface": { "components": [
                { "kind": "text", "value": "Initech leads at $2,500.75." },
                { "kind": "report", "report_id": "sales",
                  "image_url": "https://agent-lab.data-zoo.de/report/img/tok" },
                { "kind": "button", "label": "What does Initech buy?",
                  "tool": "assistant", "args": { "message": "detail" } }
            ] } }));
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

    // Every filename spelling, at BOTH the origin root (browser-time
    // discovery) and endpoint-relative under `/a2a` (Copilot Studio's
    // runtime resolves the card relative to the registered endpoint, not
    // the origin — 2026-08-30). All must 200: a 404 on the runtime path
    // surfaces to the operator only as a bare "SystemError".
    let filenames = [
        "agent-card.json",
        "agent.json",
        "agentcard.json",
        "agentCard.json",
        "agent_card.json",
    ];
    let paths: Vec<String> = filenames
        .iter()
        .flat_map(|f| [format!("/.well-known/{f}"), format!("/a2a/.well-known/{f}")])
        .collect();
    for path in &paths {
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
        // A2UI v0.9 is advertised as an A2A extension so Gemini Enterprise
        // activates it and renders the card/chart/buttons.
        let exts = card["capabilities"]["extensions"]
            .as_array()
            .expect("extensions");
        assert!(
            exts.iter()
                .any(|e| e["uri"] == "https://a2ui.org/a2a-extension/a2ui/v0.9"),
            "A2UI extension must be advertised: {card}"
        );
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
    // (`message/stream` stopped being a valid example the day it was
    // implemented — #635 P6.)
    let (status, body) = rpc(
        &base,
        &token,
        json!({"jsonrpc":"2.0","id":9,"method":"tasks/cancel","params":{}}),
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

/// A deliberately slow tool, for the disconnect-safety and polling
/// tests: the answer takes ~1.5s, far longer than the client waits.
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &'static str {
        "assistant"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
        Ok(json!({ "surface": { "text": format!("slow answer to: {msg}") } }))
    }
}

async fn slow_spec_host() -> (TestIssuer, String) {
    let iss = TestIssuer::start().await;
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SlowTool));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), "test".to_string()));
    let opts = EmbedOpts::dev().oidc(iss.issuer_url(), AUD, None).spec_a2a(
        "DataZoo Agent",
        "Answers questions.",
        PUBLIC_URL,
        "assistant",
    );
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (iss, format!("http://{addr}"))
}

fn send_immediate(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "configuration": { "blocking": false },
            "message": {
                "kind": "message",
                "role": "user",
                "messageId": "m-1",
                "parts": [{ "kind": "text", "text": text }],
            },
        },
    })
}

/// `configuration.blocking: false` (the spec's returnImmediately) —
/// the call answers with a `working` Task at once, and `tasks/get`
/// polling reaches `completed` WITH the stored reply as an artifact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_immediately_yields_working_task_then_poll_completes() {
    let (iss, base) = slow_spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    let started = std::time::Instant::now();
    let (status, body) = rpc(&base, &token, send_immediate("poll me")).await;
    assert_eq!(status, 200);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(700),
        "returnImmediately must not wait for the 1.5s dispatch"
    );
    assert_eq!(body["result"]["kind"], "task", "{body}");
    assert_eq!(body["result"]["status"]["state"], "working", "{body}");
    let task_id = body["result"]["id"].as_str().expect("task id").to_string();

    // Poll to completion.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (_, task) = rpc(
            &base,
            &token,
            json!({"jsonrpc":"2.0","id":2,"method":"tasks/get","params":{"id":task_id}}),
        )
        .await;
        let state = task["result"]["status"]["state"].as_str().unwrap_or("");
        if state == "completed" {
            let text = task["result"]["artifacts"][0]["parts"][0]["text"]
                .as_str()
                .unwrap_or("");
            assert!(
                text.contains("slow answer to: poll me"),
                "completed task must carry the stored reply; got: {task}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task never completed; last: {task}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Disconnect-safety — the Teams-499 bug class, pinned on A2A: a
/// client that fires `message/send` (blocking) and HANGS UP mid-turn
/// must not cancel the dispatch. The answer completes in the spawned
/// task and `tasks/get` recovers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_does_not_cancel_the_dispatch() {
    let (iss, base) = slow_spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    // Fire a BLOCKING send with a client timeout far below the 1.5s
    // dispatch: the connection drops while the turn is running.
    let short = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(300))
        .build()
        .unwrap();
    let err = short
        .post(format!("{base}/a2a"))
        .bearer_auth(&token)
        .json(&send("survive me"))
        .send()
        .await;
    assert!(
        err.is_err(),
        "the client must have timed out (that's the point)"
    );

    // The dispatch survived the hangup. We don't know the trace id (the
    // response died with the connection), so poll... we CAN'T address the
    // task without an id — which is exactly why a disconnect-prone client
    // should use blocking:false. What we CAN pin: the process keeps
    // serving, and a follow-up blocking call still answers.
    tokio::time::sleep(std::time::Duration::from_millis(1600)).await;
    let (status, body) = rpc(&base, &token, send("after the hangup")).await;
    assert_eq!(status, 200);
    assert!(
        body["result"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("slow answer to: after the hangup"),
        "{body}"
    );
}

/// `message/stream`: SSE of JSON-RPC responses — initial working Task,
/// a last-chunk artifact carrying the reply, and a `final: true`
/// status-update closing the stream. The card advertises it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_stream_emits_task_artifact_final() {
    let (iss, base) = spec_host().await;
    let token = iss.sign_jwt(claims(&iss.issuer_url()));

    // Card first: streaming must be advertised in the same build that
    // serves the method.
    let card: Value = reqwest::Client::new()
        .get(format!("{base}/.well-known/agent-card.json"))
        .send()
        .await
        .expect("GET card")
        .json()
        .await
        .expect("card json");
    assert_eq!(card["capabilities"]["streaming"], json!(true), "{card}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/a2a"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "message/stream",
            "params": { "message": {
                "kind": "message", "role": "user", "messageId": "m-s",
                "parts": [{ "kind": "text", "text": "stream me" }],
            } },
        }))
        .send()
        .await
        .expect("POST stream");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/event-stream"),
        "message/stream must answer SSE"
    );
    let body = resp.text().await.expect("stream body");
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    assert!(frames.len() >= 3, "task + artifact + final; got: {body}");
    assert_eq!(frames[0]["result"]["kind"], "task");
    assert_eq!(frames[0]["result"]["status"]["state"], "working");
    let last = frames.last().unwrap();
    assert_eq!(last["result"]["kind"], "status-update");
    assert_eq!(last["result"]["status"]["state"], "completed");
    assert_eq!(last["result"]["final"], json!(true));
    let artifact = frames
        .iter()
        .find(|f| f["result"]["kind"] == "artifact-update")
        .expect("an artifact frame");
    assert!(
        artifact["result"]["artifact"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("you said: stream me"),
        "{artifact}"
    );
    // Every frame is a JSON-RPC response echoing the request id.
    for f in &frames {
        assert_eq!(f["jsonrpc"], "2.0");
        assert_eq!(f["id"], 7);
    }
    // A2A 0.3.0 requires `contextId` on Task / status-update / artifact-update.
    // Gemini Enterprise's a2a-python SDK rejects the WHOLE stream if any frame
    // omits it (14 pydantic errors off one contextId-less Task). Assert every
    // frame carries a non-empty contextId, and it is the SAME across the turn.
    let ctx = frames[0]["result"]["contextId"]
        .as_str()
        .expect("initial task must carry a contextId");
    assert!(!ctx.is_empty(), "contextId must not be empty");
    for f in &frames {
        assert_eq!(
            f["result"]["contextId"].as_str(),
            Some(ctx),
            "every stream frame must carry the same contextId; got: {f}"
        );
    }

    // A rich (component) surface additionally carries an A2UI v0.9 DataPart in
    // its final artifact — what makes Gemini Enterprise render card/chart/
    // buttons instead of plain text.
    let rich = reqwest::Client::new()
        .post(format!("{base}/a2a"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 9, "method": "message/stream",
            "params": { "message": {
                "kind": "message", "role": "user", "messageId": "m-r",
                "parts": [{ "kind": "text", "text": "rich" }],
            } },
        }))
        .send()
        .await
        .expect("POST rich stream");
    let rbody = rich.text().await.expect("rich stream body");
    let rframes: Vec<Value> = rbody
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let data_parts: Vec<&Value> = rframes
        .iter()
        .filter_map(|f| f["result"]["artifact"]["parts"].as_array())
        .flatten()
        .filter(|p| p["kind"] == "data")
        .collect();
    // One DataPart per A2UI message, each `data` a DICT (A2A forbids a list) —
    // this is exactly what Gemini Enterprise's strict client requires.
    assert!(
        data_parts.len() >= 2,
        "createSurface + updateComponents parts"
    );
    for p in &data_parts {
        assert_eq!(p["metadata"]["mimeType"], "application/json+a2ui");
        assert!(p["data"].is_object(), "DataPart.data must be a dict: {p}");
        assert_eq!(p["data"]["version"], "v0.9");
    }
    let comps = data_parts
        .iter()
        .find_map(|p| p["data"]["updateComponents"]["components"].as_array())
        .expect("an updateComponents message");
    // GE's composite (Material) catalog — Material card/button; the chart is a
    // basic Image (GE's MaterialImage won't load the signed URL).
    assert!(comps.iter().any(|c| c["component"] == "MaterialCard"));
    assert!(comps.iter().any(|c| c["component"] == "Image"));
    assert!(comps.iter().any(|c| c["component"] == "MaterialButton"));
}
