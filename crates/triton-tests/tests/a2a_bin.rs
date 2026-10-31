//! Spec A2A in the `triton` binary, protocol 0.3 AND 1.0.
//!
//! The binary serves the published A2A protocol on one endpoint in two
//! versions, selected per request by `A2A-Version` (header or query):
//! absent or `0.3` → 0.3 (JSON-RPC `message/send`, `tasks/get`, …),
//! `1.0` → 1.0 (JSON-RPC `SendMessage`, `GetTask`, `ListTasks`,
//! `CancelTask`, and the HTTP+JSON binding under `/v1`), anything else →
//! VersionNotSupported (`-32009`). The card is 1.0 at
//! `/.well-known/agent-card.json` and 0.3 at `/.well-known/agent.json` or
//! `agent-card.json?version=0.3`; its URL is the configured public URL
//! (Gemini drops non-443 ports, so a bound socket address is useless).
//!
//! Cases ported from a portfolio agent's A2A live tests (protocol behaviour,
//! error codes and forms); every card, task, event and error is validated
//! against the vendored official 0.3.0 and 1.0.0 schemas.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

use crate::a2a_schema::{check03, check10};

const PUBLIC: &str = "https://agents.example.test/a2a";
const ANSWER: &str = "Three agents are planned.";

async fn agent() -> FakeAgent {
    FakeAgent::start_returning(json!({ "surface": { "components": [
        { "kind": "text", "value": ANSWER }
    ] } }))
    .await
}

fn a2a_env(agent: &FakeAgent, single_port: bool) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), single_port.to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ask_portfolio_agent={}", agent.host_port()),
        ),
        ("TRITON_A2A_PUBLIC_URL".to_string(), PUBLIC.to_string()),
        (
            "TRITON_A2A_DEFAULT_TOOL".to_string(),
            "ask_portfolio_agent".to_string(),
        ),
        (
            "TRITON_A2A_AGENT_NAME".to_string(),
            "Portfolio agent".to_string(),
        ),
        (
            "TRITON_A2A_AGENT_DESCRIPTION".to_string(),
            "Answers questions about the portfolio.".to_string(),
        ),
    ])
}

struct Host {
    triton: TritonProcess,
    _agent: FakeAgent,
}

impl Host {
    /// The JSON-RPC endpoint (single-port: nested at `/a2a`).
    fn rpc_url(&self) -> String {
        self.triton.rest_url("/a2a")
    }
    fn url(&self, path: &str) -> String {
        self.triton.rest_url(path)
    }
}

async fn host() -> Host {
    let agent = agent().await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(&agent, true)).await;
    Host {
        triton,
        _agent: agent,
    }
}

async fn rpc(
    url: &str,
    token: Option<&str>,
    version: Option<&str>,
    method: &str,
    params: Value,
) -> (reqwest::StatusCode, Value) {
    let mut req = reqwest::Client::new().post(url).json(&json!({
        "jsonrpc": "2.0", "id": 7, "method": method, "params": params,
    }));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    let resp = req.send().await.expect("POST json-rpc");
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn send03(text: &str, message_id: &str) -> Value {
    json!({ "message": {
        "kind": "message", "role": "user", "messageId": message_id,
        "parts": [{ "kind": "text", "text": text }],
    } })
}

fn send10(text: &str, message_id: &str) -> Value {
    json!({ "message": {
        "role": "ROLE_USER", "messageId": message_id,
        "parts": [{ "text": text }],
    } })
}

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

const DEV: Option<&str> = Some("dev-token");

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cards_are_served_in_both_versions_and_validate() {
    let h = host().await;

    let modern: Value = reqwest::get(h.url("/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check10("AgentCard", &modern);
    assert_eq!(modern["name"], "Portfolio agent");
    let interfaces = modern["supportedInterfaces"]
        .as_array()
        .expect("interfaces");
    let has = |binding: &str, url: &str, version: &str| {
        interfaces.iter().any(|i| {
            i["protocolBinding"] == binding && i["url"] == url && i["protocolVersion"] == version
        })
    };
    assert!(has("JSONRPC", PUBLIC, "1.0"), "{modern:#}");
    assert!(
        has("HTTP+JSON", &format!("{PUBLIC}/v1"), "1.0"),
        "{modern:#}"
    );
    assert!(has("JSONRPC", PUBLIC, "0.3"), "{modern:#}");
    assert_eq!(
        interfaces[0]["protocolVersion"], "1.0",
        "the first interface is the preferred one"
    );
    assert_eq!(modern["capabilities"]["streaming"], true);
    assert_eq!(modern["capabilities"]["pushNotifications"], false);
    let skills = modern["skills"].as_array().expect("skills");
    assert!(
        skills.iter().any(|s| s["id"] == "ask_portfolio_agent"),
        "the default tool is the skill: {modern:#}"
    );

    let explicit: Value = reqwest::get(h.url("/.well-known/agent-card.json?version=1.0"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explicit, modern, "?version=1.0 is the default");

    for path in [
        "/.well-known/agent-card.json?version=0.3",
        "/.well-known/agent.json",
        "/a2a/.well-known/agent.json",
        "/a2a/.well-known/agent-card.json?version=0.3",
    ] {
        let legacy: Value = reqwest::get(h.url(path))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        check03("AgentCard", &legacy);
        assert_eq!(legacy["protocolVersion"], "0.3.0", "{path}");
        assert_eq!(
            legacy["url"], PUBLIC,
            "{path}: the public URL, not the socket"
        );
        assert_eq!(legacy["preferredTransport"], "JSONRPC");
    }
    let nested: Value = reqwest::get(h.url("/a2a/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        nested, modern,
        "the endpoint-relative copy is the same card"
    );

    let unknown = reqwest::get(h.url("/.well-known/agent-card.json?version=2.0"))
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400, "an unknown card version is refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_port_mode_serves_the_card_and_json_rpc_on_the_a2a_listener() {
    let agent = agent().await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(&agent, false)).await;
    let card: Value = reqwest::get(triton.a2a_url("/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check10("AgentCard", &card);
    let (status, body) = rpc(
        &triton.a2a_url("/"),
        DEV,
        None,
        "message/send",
        send03("Which agents are planned?", "m-3p"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    check03("SendMessageSuccessResponse", &body);
    // The Triton-shaped route keeps working beside it.
    let legacy = reqwest::Client::new()
        .post(triton.a2a_url("/message:send"))
        .bearer_auth("dev-token")
        .json(&json!({ "parts": [{ "data": { "tool": "echo", "args": { "message": "x" } } }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_rpc_0_3_send_get_and_errors_in_the_0_3_form() {
    let h = host().await;
    let url = h.rpc_url();

    let (status, sent) = rpc(
        &url,
        DEV,
        None,
        "message/send",
        send03("Which agents?", "m-1"),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    check03("SendMessageSuccessResponse", &sent);
    let result = &sent["result"];
    assert_eq!(result["kind"], "message");
    assert_eq!(result["parts"][0]["text"], ANSWER);
    let task_id = result["taskId"].as_str().expect("taskId").to_string();

    let (_, got) = rpc(
        &url,
        DEV,
        Some("0.3"),
        "tasks/get",
        json!({ "id": task_id }),
    )
    .await;
    check03("GetTaskSuccessResponse", &got);
    assert_eq!(got["result"]["status"]["state"], "completed");
    assert_eq!(got["result"]["artifacts"][0]["parts"][0]["text"], ANSWER);

    // The same task in the 1.0 form, on the same endpoint.
    let (_, modern) = rpc(&url, DEV, Some("1.0"), "GetTask", json!({ "id": task_id })).await;
    check10("Task", &modern["result"]);
    assert_eq!(modern["result"]["status"]["state"], "TASK_STATE_COMPLETED");

    let codes = [
        (
            "tasks/list",
            json!({}),
            -32601,
            "0.3 defines tasks/list for gRPC and REST only",
        ),
        (
            "GetTask",
            json!({ "id": task_id }),
            -32601,
            "0.3 has no GetTask",
        ),
        (
            "tasks/cancel",
            json!({ "id": task_id }),
            -32002,
            "tasks are not cancelable",
        ),
        (
            "tasks/pushNotificationConfig/set",
            json!({ "taskId": task_id, "pushNotificationConfig": { "url": "https://hooks.example/a2a" } }),
            -32003,
            "no push notifications",
        ),
        (
            "agent/getAuthenticatedExtendedCard",
            json!({}),
            -32007,
            "no extended card",
        ),
        (
            "tasks/get",
            json!({ "id": "no-such-task" }),
            -32001,
            "unknown task",
        ),
    ];
    for (method, params, code, why) in codes {
        let (status, body) = rpc(&url, DEV, None, method, params).await;
        assert_eq!(status, 200, "{method}: {body}");
        check03("JSONRPCErrorResponse", &body);
        assert_eq!(body["error"]["code"], code, "{why}: {body}");
    }

    let (_, unsupported) = rpc(
        &url,
        DEV,
        Some("2.0"),
        "tasks/get",
        json!({ "id": task_id }),
    )
    .await;
    check03("JSONRPCErrorResponse", &unsupported);
    assert_eq!(unsupported["error"]["code"], -32009);
    assert_eq!(
        unsupported["error"]["data"],
        json!({ "requestedVersion": "2.0", "supportedVersions": ["0.3", "1.0"] }),
        "the 0.3 form: plain data, no google.rpc.ErrorInfo"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_rpc_1_0_send_get_list_cancel_and_errors() {
    let h = host().await;
    let url = h.rpc_url();

    let (status, sent) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "SendMessage",
        send10("Which agents?", "m-10"),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    check10("SendMessageResponse", &sent["result"]);
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{sent}");
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], ANSWER);
    let task_id = task["id"].as_str().unwrap().to_string();
    let context_id = task["contextId"].as_str().unwrap().to_string();

    let (_, got) = rpc(&url, DEV, Some("1.0"), "GetTask", json!({ "id": task_id })).await;
    check10("Task", &got["result"]);
    assert_eq!(got["result"]["id"], task_id);

    let (_, listed) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "ListTasks",
        json!({ "contextId": context_id }),
    )
    .await;
    check10("ListTasksResponse", &listed["result"]);
    let tasks = listed["result"]["tasks"].as_array().expect("tasks");
    assert!(tasks.iter().any(|t| t["id"] == task_id), "{listed}");
    let (_, bad_page) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "ListTasks",
        json!({ "pageSize": 0 }),
    )
    .await;
    assert_eq!(bad_page["error"]["code"], -32602, "{bad_page}");

    let (_, cancel) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "CancelTask",
        json!({ "id": task_id }),
    )
    .await;
    assert_eq!(cancel["error"]["code"], -32002, "{cancel}");
    assert_eq!(
        cancel["error"]["data"][0]["@type"],
        "type.googleapis.com/google.rpc.ErrorInfo"
    );
    assert_eq!(cancel["error"]["data"][0]["reason"], "TASK_NOT_CANCELABLE");

    let (_, missing) = rpc(&url, DEV, Some("1.0"), "GetTask", json!({ "id": "nope" })).await;
    assert_eq!(missing["error"]["code"], -32001);
    assert_eq!(missing["error"]["data"][0]["reason"], "TASK_NOT_FOUND");

    let (_, wrong) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "tasks/get",
        json!({ "id": task_id }),
    )
    .await;
    assert_eq!(wrong["error"]["code"], -32601, "1.0 has no tasks/get");

    let (_, unsupported) = rpc(&url, DEV, Some("2.0"), "GetTask", json!({ "id": task_id })).await;
    assert_eq!(unsupported["error"]["code"], -32009);
    assert_eq!(
        unsupported["error"]["data"][0]["reason"],
        "VERSION_NOT_SUPPORTED"
    );

    let (_, multi) = rpc(
        &url,
        DEV,
        Some("1.0"),
        "SendMessage",
        json!({ "message": {
        "role": "ROLE_USER", "messageId": "m-cont", "taskId": task_id,
        "parts": [{ "text": "and now?" }],
    } }),
    )
    .await;
    assert_eq!(
        multi["error"]["code"], -32004,
        "no multi-turn: a message to an existing task is refused: {multi}"
    );

    // The query parameter selects the version as the header does.
    let (_, by_query) = rpc(
        &format!("{url}?A2A-Version=1.0"),
        DEV,
        None,
        "GetTask",
        json!({ "id": task_id }),
    )
    .await;
    assert_eq!(by_query["result"]["id"], task_id, "{by_query}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_http_json_binding_speaks_1_0_under_v1() {
    let h = host().await;
    let base = h.url("/a2a/v1");
    let client = reqwest::Client::new();

    let sent = client
        .post(format!("{base}/message:send"))
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .json(&send10("Which agents?", "m-rest"))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), 200);
    assert!(
        sent.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/a2a+json"),
        "{:?}",
        sent.headers()
    );
    let sent: Value = sent.json().await.unwrap();
    check10("SendMessageResponse", &sent);
    let id = sent["task"]["id"].as_str().unwrap().to_string();

    let got: Value = client
        .get(format!("{base}/tasks/{id}"))
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check10("Task", &got);
    assert_eq!(got["status"]["state"], "TASK_STATE_COMPLETED");

    let listed: Value = client
        .get(format!("{base}/tasks?pageSize=10&A2A-Version=1.0"))
        .bearer_auth("dev-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check10("ListTasksResponse", &listed);
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == id)
    );

    let cancel = client
        .post(format!("{base}/tasks/{id}:cancel"))
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 400);
    let cancel: Value = cancel.json().await.unwrap();
    assert_eq!(cancel["error"]["status"], "FAILED_PRECONDITION");
    assert_eq!(
        cancel["error"]["details"][0]["reason"],
        "TASK_NOT_CANCELABLE"
    );

    let missing = client
        .get(format!("{base}/tasks/no-such-task"))
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // The HTTP+JSON binding is 1.0 only: without the header it refuses.
    let unversioned = client
        .get(format!("{base}/tasks/{id}"))
        .bearer_auth("dev-token")
        .send()
        .await
        .unwrap();
    assert_eq!(unversioned.status(), 400);
    let unversioned: Value = unversioned.json().await.unwrap();
    assert_eq!(
        unversioned["error"]["details"][0]["reason"],
        "VERSION_NOT_SUPPORTED"
    );

    let unauthenticated = client
        .post(format!("{base}/message:send"))
        .header("A2A-Version", "1.0")
        .json(&send10("x", "m-anon"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_validate_in_both_versions_and_both_bindings() {
    let h = host().await;
    let client = reqwest::Client::new();

    // 1.0 JSON-RPC: each event is a JSON-RPC response around a StreamResponse.
    let body = client
        .post(h.rpc_url())
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "SendStreamingMessage",
            "params": send10("Which agents?", "m-s10") }),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_data(&body);
    assert!(events.len() >= 3, "{body}");
    for e in &events {
        assert_eq!(e["jsonrpc"], "2.0");
        check10("StreamResponse", &e["result"]);
    }
    assert!(
        events[0]["result"]["task"].is_object(),
        "the task first: {body}"
    );
    let last = &events.last().unwrap()["result"];
    assert_eq!(
        last["statusUpdate"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["result"]["artifactUpdate"]["artifact"]["parts"][0]["text"] == ANSWER),
        "{body}"
    );

    // 0.3 JSON-RPC: SendStreamingMessageSuccessResponse per event, final last.
    let body = client
        .post(h.rpc_url())
        .bearer_auth("dev-token")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 10, "method": "message/stream",
            "params": send03("Which agents?", "m-s03") }),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_data(&body);
    for e in &events {
        check03("SendStreamingMessageSuccessResponse", e);
    }
    assert_eq!(events.last().unwrap()["result"]["final"], true, "{body}");

    // 1.0 HTTP+JSON: bare StreamResponse objects.
    let body = client
        .post(h.url("/a2a/v1/message:stream"))
        .bearer_auth("dev-token")
        .header("A2A-Version", "1.0")
        .json(&send10("Which agents?", "m-srest"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = sse_data(&body);
    assert!(!events.is_empty(), "{body}");
    for e in &events {
        check10("StreamResponse", e);
    }
    assert_eq!(
        events.last().unwrap()["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_oidc_the_cards_name_the_credential_and_tasks_stay_with_their_owner() {
    let agent = agent().await;
    let iss = TestIssuer::start().await;
    let mut env = a2a_env(&agent, true);
    env.insert("TRITON_OIDC_ISSUER".into(), iss.issuer_url());
    env.insert("TRITON_OIDC_AUDIENCE".into(), "a2a-aud".into());
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let token = |sub: &str, tenant: &str| {
        iss.sign_jwt(
            json!({ "iss": iss.issuer_url(), "aud": "a2a-aud", "sub": sub,
            "tenant": tenant, "exp": now() + 600 }),
        )
    };

    let modern: Value = reqwest::get(triton.rest_url("/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check10("AgentCard", &modern);
    assert_eq!(
        modern["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"], "bearer",
        "{modern:#}"
    );
    assert_eq!(
        modern["securitySchemes"]["oidc_0"]["openIdConnectSecurityScheme"]["openIdConnectUrl"],
        format!("{}/.well-known/openid-configuration", iss.issuer_url())
    );
    let legacy: Value = reqwest::get(triton.rest_url("/.well-known/agent.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check03("AgentCard", &legacy);
    assert_eq!(legacy["security"], json!([{ "bearer": [] }]));

    let url = triton.rest_url("/a2a");
    let (status, _) = rpc(&url, None, Some("1.0"), "GetTask", json!({ "id": "x" })).await;
    assert_eq!(status, 401);

    let alice = token("alice", "acme");
    let (_, sent) = rpc(
        &url,
        Some(&alice),
        Some("1.0"),
        "SendMessage",
        send10("Which agents?", "m-own"),
    )
    .await;
    let id = sent["result"]["task"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let (_, own) = rpc(
        &url,
        Some(&alice),
        Some("1.0"),
        "GetTask",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(own["result"]["id"], id, "{own}");

    let mallory = token("mallory", "globex");
    let (_, foreign) = rpc(
        &url,
        Some(&mallory),
        Some("1.0"),
        "GetTask",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(
        foreign["error"]["code"], -32001,
        "another tenant's task is not found: {foreign}"
    );
    let (_, listed) = rpc(&url, Some(&mallory), Some("1.0"), "ListTasks", json!({})).await;
    assert_eq!(listed["result"]["tasks"], json!([]), "{listed}");
}
