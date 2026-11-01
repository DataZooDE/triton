//! A dedicated A2A 0.3 endpoint base: `<endpoint>/v0.3`.
//!
//! Some discovery clients accept only a 0.3 card and read the A2A spec's
//! `agent-card.json` spelling first, refusing the 1.0 card served there
//! instead of falling back to `agent.json` (Microsoft Copilot Studio:
//! "This agent card uses A2A protocol v1, which is not supported yet").
//! Such a client is given `<endpoint>/v0.3`: every card spelling under
//! `<endpoint>/v0.3/.well-known/` is the 0.3 card whose `url` is
//! `<endpoint>/v0.3`, and `POST <endpoint>/v0.3` is the 0.3 JSON-RPC
//! binding — the same handler, auth and task store as `<endpoint>`, but
//! pinned to 0.3 (`A2A-Version: 1.0` is VersionNotSupported).
//!
//! No mocks: the real binary, real HTTP, a real token issuer.

use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use triton_tests::{TestIssuer, TritonProcess};

use crate::a2a_bin::{ANSWER, PUBLIC, a2a_env, agent, now};
use crate::a2a_schema::check03;

const ORIGIN: &str = "https://copilotstudio.microsoft.com";
const SPELLINGS: &[&str] = &[
    "agent-card.json",
    "agent.json",
    "agentcard.json",
    "agentCard.json",
    "agent_card.json",
];

async fn rpc(
    url: &str,
    token: Option<&str>,
    version: Option<&str>,
    method: &str,
    params: Value,
) -> (reqwest::StatusCode, Value) {
    let mut req = reqwest::Client::new().post(url).json(&json!({
        "jsonrpc": "2.0", "id": 3, "method": method, "params": params,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_card_spelling_under_the_03_base_is_the_03_card_pointing_at_it() {
    let agent = agent().await;
    let iss = TestIssuer::start().await;
    let mut env = a2a_env(&agent, true);
    env.insert("TRITON_OIDC_ISSUER".into(), iss.issuer_url());
    env.insert("TRITON_OIDC_AUDIENCE".into(), "a2a-aud".into());
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let client = reqwest::Client::new();

    let reference: Value = reqwest::get(triton.rest_url("/a2a/.well-known/agent.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    for name in SPELLINGS {
        let path = format!("/a2a/v0.3/.well-known/{name}");
        let resp = client
            .get(triton.rest_url(&path))
            .header("origin", ORIGIN)
            .send()
            .await
            .expect("GET card");
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .map(|v| v.to_str().unwrap()),
            Some("*"),
            "{path}"
        );
        let card: Value = resp.json().await.expect("card JSON");
        check03("AgentCard", &card);
        assert_eq!(card["protocolVersion"], "0.3.0", "{path}: {card:#}");
        assert_eq!(card["url"], format!("{PUBLIC}/v0.3"), "{path}");
        assert_eq!(card["preferredTransport"], "JSONRPC", "{path}");
        assert_eq!(card["security"], json!([{ "bearer": [] }]), "{path}");
        assert_eq!(
            card["securitySchemes"]["oidc_0"]["type"], "openIdConnect",
            "{path}: {card:#}"
        );
        // A plain-text card: no streaming, no extensions advertised
        // (message/stream stays callable, see below).
        assert_eq!(card["capabilities"]["streaming"], false, "{path}: {card:#}");
        assert_eq!(
            card["capabilities"].get("extensions"),
            None,
            "{path}: {card:#}"
        );
        assert_eq!(card["defaultInputModes"], json!(["text/plain"]), "{path}");
        assert_eq!(card["defaultOutputModes"], json!(["text/plain"]), "{path}");
        // Everything else is the existing 0.3 card.
        let mut same = card.clone();
        same["url"] = reference["url"].clone();
        same["capabilities"] = reference["capabilities"].clone();
        assert_eq!(same, reference, "{path}");

        let preflight = client
            .request(Method::OPTIONS, triton.rest_url(&path))
            .header("origin", ORIGIN)
            .header("access-control-request-method", "GET")
            .header("access-control-request-private-network", "true")
            .send()
            .await
            .expect("OPTIONS card");
        assert_eq!(preflight.status(), 204, "{path}");
        assert_eq!(
            preflight
                .headers()
                .get("access-control-allow-private-network")
                .map(|v| v.to_str().unwrap()),
            Some("true"),
            "{path}"
        );
    }

    // Pinned: no version query turns the 0.3 base's card into 1.0.
    let pinned: Value =
        reqwest::get(triton.rest_url("/a2a/v0.3/.well-known/agent-card.json?version=1.0"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(pinned["protocolVersion"], "0.3.0", "{pinned:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_03_base_is_the_authenticated_03_json_rpc_binding_only() {
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
    let url = triton.rest_url("/a2a/v0.3");

    let (status, _) = rpc(&url, None, None, "message/send", send03("Hi", "m-0")).await;
    assert_eq!(status, 401, "same auth as /a2a");

    let alice = token("alice", "acme");
    let (status, sent) = rpc(
        &url,
        Some(&alice),
        None,
        "message/send",
        send03("Which agents?", "m-1"),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    let result = &sent["result"];
    check03("Message", result);
    assert_eq!(result["kind"], "message", "{sent}");
    let text = result["parts"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains(ANSWER), "{sent}");
    let task_id = result["taskId"].as_str().expect("taskId").to_string();

    // Same task store as /a2a, same owner scoping.
    for base in ["/a2a/v0.3", "/a2a"] {
        let (_, got) = rpc(
            &triton.rest_url(base),
            Some(&alice),
            None,
            "tasks/get",
            json!({ "id": task_id }),
        )
        .await;
        assert_eq!(got["result"]["id"], task_id, "{base}: {got}");
    }
    let (_, foreign) = rpc(
        &url,
        Some(&token("mallory", "globex")),
        None,
        "tasks/get",
        json!({ "id": task_id }),
    )
    .await;
    assert_eq!(foreign["error"]["code"], -32001, "{foreign}");

    // Streaming is the 0.3 `message/stream`.
    let resp = reqwest::Client::new()
        .post(&url)
        .bearer_auth(&alice)
        .json(
            &json!({ "jsonrpc": "2.0", "id": 4, "method": "message/stream",
            "params": send03("Which agents?", "m-2") }),
        )
        .send()
        .await
        .expect("POST stream");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"final\":true"), "{body}");

    // Pinned to 0.3: a 1.0 request is refused in the 0.3 form.
    let (_, v1) = rpc(
        &url,
        Some(&alice),
        Some("1.0"),
        "SendMessage",
        json!({ "message": { "role": "ROLE_USER", "messageId": "m-3",
            "parts": [{ "text": "Hi" }] } }),
    )
    .await;
    assert_eq!(v1["error"]["code"], -32009, "{v1}");
    let (_, v1_name) = rpc(&url, Some(&alice), None, "SendMessage", json!({})).await;
    assert_eq!(v1_name["error"]["code"], -32601, "{v1_name}");

    // The 1.0 HTTP+JSON binding is not under the 0.3 base.
    let rest = reqwest::Client::new()
        .post(triton.rest_url("/a2a/v0.3/v1/message:send"))
        .bearer_auth(&alice)
        .header("A2A-Version", "1.0")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_port_mode_serves_the_03_base_on_the_a2a_listener() {
    let agent = agent().await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(&agent, false)).await;
    let card: Value = reqwest::get(triton.a2a_url("/v0.3/.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(card["protocolVersion"], "0.3.0", "{card:#}");
    assert_eq!(card["url"], format!("{PUBLIC}/v0.3"));
    let (status, sent) = rpc(
        &triton.a2a_url("/v0.3"),
        Some("dev-token"),
        None,
        "message/send",
        send03("Which agents?", "m-1"),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["result"]["kind"], "message", "{sent}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_endpoint_cards_keep_streaming_and_the_a2ui_extension() {
    let agent = agent().await;
    let triton =
        TritonProcess::spawn_with_env(Duration::from_secs(10), a2a_env(&agent, true)).await;
    for path in ["/a2a/.well-known/agent.json", "/.well-known/agent.json"] {
        let card: Value = reqwest::get(triton.rest_url(path))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(card["capabilities"]["streaming"], true, "{path}");
        assert!(
            card["capabilities"]["extensions"]
                .as_array()
                .is_some_and(|e| !e.is_empty()),
            "{path}: {card:#}"
        );
    }
}
