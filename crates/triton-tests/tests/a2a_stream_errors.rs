//! A streaming method is always answered as an event stream, errors included.
//!
//! A2A 0.3.0 §3.3.1 and §7: for `message/stream` (and `tasks/resubscribe`)
//! the server answers HTTP 200 with `Content-Type: text/event-stream`, and
//! every SSE `data` is a `SendStreamingMessageResponse` — which the 0.3.0
//! types define as `SendStreamingMessageSuccessResponse |
//! JSONRPCErrorResponse`. A2A 1.0 §9.1 / §9.4.2 keep that for
//! `SendStreamingMessage` over JSON-RPC. The official clients open the
//! answer as SSE and refuse anything else ("Expected response header
//! Content-Type to contain 'text/event-stream', got 'application/json'"),
//! so an error that comes back as a plain JSON-RPC body never reaches the
//! user as an error: the client reports a protocol fault instead.
//!
//! Only authentication keeps its HTTP status (401/403): a caller's
//! middleware acts on it before reading any body. The HTTP+JSON binding
//! (1.0 §11.6) maps errors to HTTP statuses with a `google.rpc.Status`
//! body, and its `StreamResponse` has no error member, so a rejected
//! request there stays an HTTP error.
//!
//! A message whose text parts are all empty (a host opening the
//! conversation) is not an error at all: it gets a short agent message,
//! the same on `message/send` and `message/stream`, and the upstream is
//! not called.
//!
//! No mocks: the real binary, real HTTP, a fake upstream agent.

use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::FakeAgent;

use crate::a2a_bin::{a2a_env, agent};
use crate::a2a_schema::{check03, check10};

const A2UI: &str = triton_core::a2ui::ge::EXTENSION_URI;

struct Answer {
    status: u16,
    content_type: String,
    events: Vec<Value>,
    raw: String,
}

async fn post(url: &str, version: Option<&str>, a2ui: bool, body: Value) -> Answer {
    let mut req = reqwest::Client::new()
        .post(url)
        .bearer_auth("dev-token")
        .header("accept", "text/event-stream")
        .json(&body);
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    if a2ui {
        req = req.header("X-A2A-Extensions", A2UI);
    }
    let resp = req.send().await.expect("POST");
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let raw = resp.text().await.unwrap();
    let events = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect();
    Answer {
        status,
        content_type,
        events,
        raw,
    }
}

fn assert_sse(a: &Answer, what: &str) {
    assert_eq!(a.status, 200, "{what}: {}", a.raw);
    assert!(
        a.content_type.starts_with("text/event-stream"),
        "{what}: a streaming method must answer SSE, got `{}`: {}",
        a.content_type,
        a.raw
    );
    assert!(!a.events.is_empty(), "{what}: no event: {}", a.raw);
}

/// Gemini Enterprise opens a conversation with an empty text part.
fn empty03() -> Value {
    json!({ "message": { "kind": "message", "role": "user", "messageId": "m-empty",
        "parts": [{ "kind": "text", "text": "" }] } })
}

fn empty10() -> Value {
    json!({ "message": { "role": "ROLE_USER", "messageId": "m-empty",
        "parts": [{ "text": "  " }] } })
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn reply_text03(result: &Value) -> String {
    assert_eq!(result["kind"], "message", "{result}");
    assert_eq!(result["role"], "agent", "{result}");
    result["parts"][0]["text"].as_str().unwrap().to_string()
}

fn reply_text10(result: &Value) -> String {
    let m = &result["message"];
    assert_eq!(m["role"], "ROLE_AGENT", "{result}");
    m["parts"][0]["text"].as_str().unwrap().to_string()
}

async fn spawn(agent: &FakeAgent) -> TritonProcess {
    TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(agent, true)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_message_gets_a_short_reply_on_every_streaming_binding() {
    let agent = agent().await;
    let triton = spawn(&agent).await;
    let a2a = triton.rest_url("/a2a");
    let v03 = triton.rest_url("/a2a/v0.3");

    let mut texts = Vec::new();
    // 0.3 JSON-RPC, on the endpoint and on the dedicated 0.3 base, with and
    // without the A2UI extension (Gemini Enterprise activates it).
    for (url, a2ui) in [(&a2a, false), (&a2a, true), (&v03, false), (&v03, true)] {
        let a = post(url, None, a2ui, rpc(1, "message/stream", empty03())).await;
        let what = format!("0.3 message/stream at {url} (a2ui {a2ui})");
        assert_sse(&a, &what);
        assert_eq!(a.events.len(), 1, "{what}: {}", a.raw);
        check03("SendStreamingMessageSuccessResponse", &a.events[0]);
        texts.push(reply_text03(&a.events[0]["result"]));
    }
    // 1.0 JSON-RPC.
    let a = post(
        &a2a,
        Some("1.0"),
        false,
        rpc(2, "SendStreamingMessage", empty10()),
    )
    .await;
    assert_sse(&a, "1.0 SendStreamingMessage");
    assert_eq!(a.events.len(), 1, "{}", a.raw);
    check10("StreamResponse", &a.events[0]["result"]);
    texts.push(reply_text10(&a.events[0]["result"]));
    // 1.0 HTTP+JSON.
    let a = post(
        &triton.rest_url("/a2a/v1/message:stream"),
        Some("1.0"),
        false,
        empty10(),
    )
    .await;
    assert_sse(&a, "1.0 REST message:stream");
    check10("StreamResponse", &a.events[0]);
    texts.push(reply_text10(&a.events[0]));

    // The non-streaming methods answer the same way.
    let a = post(&a2a, None, false, rpc(3, "message/send", empty03())).await;
    assert_eq!(a.status, 200, "{}", a.raw);
    let body: Value = serde_json::from_str(&a.raw).unwrap();
    texts.push(reply_text03(&body["result"]));
    let a = post(&a2a, Some("1.0"), false, rpc(4, "SendMessage", empty10())).await;
    let body: Value = serde_json::from_str(&a.raw).unwrap();
    check10("SendMessageResponse", &body["result"]);
    texts.push(reply_text10(&body["result"]));
    let a = post(
        &triton.rest_url("/a2a/v1/message:send"),
        Some("1.0"),
        false,
        empty10(),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.raw);
    let body: Value = serde_json::from_str(&a.raw).unwrap();
    texts.push(reply_text10(&body));

    assert!(!texts[0].trim().is_empty());
    assert!(texts.iter().all(|t| t == &texts[0]), "{texts:?}");
    assert!(
        agent.bodies_seen().is_empty(),
        "an empty message must not reach the upstream: {:?}",
        agent.bodies_seen()
    );
}

/// Errors after authentication arrive as one JSON-RPC error event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streaming_request_error_is_an_error_event() {
    let agent = agent().await;
    let triton = spawn(&agent).await;
    let a2a = triton.rest_url("/a2a");
    let v03 = triton.rest_url("/a2a/v0.3");

    let no_parts03 = json!({ "message": { "kind": "message", "role": "user",
        "messageId": "m-1", "parts": [] } });
    let no_parts10 = json!({ "message": { "role": "ROLE_USER", "messageId": "m-1", "parts": [] } });
    let unknown_task10 = json!({ "message": { "role": "ROLE_USER", "messageId": "m-2",
        "taskId": "no-such-task", "parts": [{ "text": "hi" }] } });
    let cases: Vec<(&str, &String, Option<&str>, Value, i64)> = vec![
        (
            "0.3 invalid params",
            &a2a,
            None,
            rpc(1, "message/stream", no_parts03.clone()),
            -32602,
        ),
        (
            "0.3 base invalid params",
            &v03,
            None,
            rpc(2, "message/stream", no_parts03.clone()),
            -32602,
        ),
        (
            "0.3 wrong jsonrpc version",
            &a2a,
            None,
            json!({ "jsonrpc": "1.0", "id": 3, "method": "message/stream", "params": {} }),
            -32600,
        ),
        (
            "0.3 unsupported A2A version",
            &a2a,
            Some("9.9"),
            rpc(4, "message/stream", empty03()),
            -32009,
        ),
        (
            "0.3 base refuses 1.0",
            &v03,
            Some("1.0"),
            rpc(5, "message/stream", empty03()),
            -32009,
        ),
        (
            "1.0 invalid params",
            &a2a,
            Some("1.0"),
            rpc(6, "SendStreamingMessage", no_parts10),
            -32602,
        ),
        (
            "1.0 unknown task",
            &a2a,
            Some("1.0"),
            rpc(7, "SendStreamingMessage", unknown_task10),
            -32001,
        ),
        (
            "1.0 unsupported A2A version",
            &a2a,
            Some("9.9"),
            rpc(8, "SendStreamingMessage", empty10()),
            -32009,
        ),
        (
            "0.3 resubscribe without id",
            &a2a,
            None,
            rpc(9, "tasks/resubscribe", json!({})),
            -32602,
        ),
        (
            "1.0 subscribe to an unknown task",
            &a2a,
            Some("1.0"),
            rpc(10, "SubscribeToTask", json!({ "id": "no-such-task" })),
            -32001,
        ),
    ];
    for (what, url, version, body, code) in cases {
        let id = body["id"].clone();
        let a = post(url, version, false, body).await;
        assert_sse(&a, what);
        assert_eq!(a.events.len(), 1, "{what}: {}", a.raw);
        let e = &a.events[0];
        assert_eq!(e["jsonrpc"], "2.0", "{what}: {e}");
        assert_eq!(e["id"], id, "{what}: {e}");
        assert_eq!(e["error"]["code"], code, "{what}: {e}");
        assert!(e.get("result").is_none(), "{what}: {e}");
        if version != Some("1.0") {
            check03("JSONRPCErrorResponse", e);
        }
    }

    // Authentication keeps its HTTP status.
    let resp = reqwest::Client::new()
        .post(&a2a)
        .json(&rpc(11, "message/stream", empty03()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // The HTTP+JSON binding maps a rejected request to an HTTP status
    // (1.0 §11.6); its stream has no error event.
    let a = post(
        &triton.rest_url("/a2a/v1/message:stream"),
        Some("1.0"),
        false,
        json!({ "message": { "role": "ROLE_USER", "messageId": "m-3", "parts": [] } }),
    )
    .await;
    assert_eq!(a.status, 400, "{}", a.raw);
    assert!(agent.bodies_seen().is_empty());
}

/// An upstream that cannot be reached fails the task inside the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_upstream_fails_the_streamed_task() {
    let agent = agent().await;
    let mut env = a2a_env(&agent, true);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    env.insert(
        "TRITON_STATIC_UPSTREAMS".into(),
        format!("ask_portfolio_agent=127.0.0.1:{port}"),
    );
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let a2a = triton.rest_url("/a2a");
    let text03 = json!({ "message": { "kind": "message", "role": "user", "messageId": "m-u",
        "parts": [{ "kind": "text", "text": "Which agents?" }] } });
    let text10 = json!({ "message": { "role": "ROLE_USER", "messageId": "m-u",
        "parts": [{ "text": "Which agents?" }] } });

    let a = post(&a2a, None, true, rpc(1, "message/stream", text03)).await;
    assert_sse(&a, "0.3 unreachable upstream");
    let last = a.events.last().unwrap();
    check03("SendStreamingMessageResponse", last);
    assert!(
        last["result"]["status"]["state"] == "failed" || last.get("error").is_some(),
        "{}",
        a.raw
    );

    let a = post(
        &a2a,
        Some("1.0"),
        false,
        rpc(2, "SendStreamingMessage", text10),
    )
    .await;
    assert_sse(&a, "1.0 unreachable upstream");
    let last = a.events.last().unwrap();
    assert_eq!(
        last["result"]["statusUpdate"]["status"]["state"], "TASK_STATE_FAILED",
        "{}",
        a.raw
    );
}
