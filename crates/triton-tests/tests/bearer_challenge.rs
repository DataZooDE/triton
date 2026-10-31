//! A caller without a valid credential gets HTTP 401 with an RFC 6750
//! `WWW-Authenticate: Bearer` challenge on every face of the single port:
//! spec A2A (JSON-RPC and the HTTP+JSON binding) and MCP.
//!
//! Why: MCP clients (claude.ai, ChatGPT) start their OAuth flow only on an
//! HTTP 401 with a `WWW-Authenticate` header (MCP authorization, "servers
//! MUST use the WWW-Authenticate header"), and A2A client libraries act on
//! the HTTP status. An MCP auth failure used to be a `200` carrying JSON-RPC
//! `-32001`, which such a client reads as a working server. The JSON-RPC
//! body (code and id) is kept, so a client that reads only the body sees
//! the same error as before.
//!
//! RFC 6750 §3: a request without credentials gets the bare challenge (no
//! `error`); a request with a credential that does not verify gets
//! `error="invalid_token"`.
//!
//! No mocks: the real binary with an OIDC issuer (`TestIssuer`) and a
//! `FakeAgent` upstream, the shape of a public Triton.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const PUBLIC: &str = "https://agents.example.test/a2a";

async fn post(
    url: &str,
    token: Option<&str>,
    body: Value,
) -> (reqwest::StatusCode, Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.expect("POST");
    let status = resp.status();
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().expect("ASCII challenge").to_string());
    (status, challenge, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthenticated_calls_get_401_with_a_bearer_challenge_on_a2a_and_mcp() {
    let agent = FakeAgent::start_returning(json!({ "surface": { "components": [
        { "kind": "text", "value": "ok" }
    ] } }))
    .await;
    let iss = TestIssuer::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ask_portfolio_agent={}", agent.host_port()),
        ),
        ("TRITON_A2A_PUBLIC_URL".to_string(), PUBLIC.to_string()),
        (
            "TRITON_A2A_DEFAULT_TOOL".to_string(),
            "ask_portfolio_agent".to_string(),
        ),
        ("TRITON_OIDC_ISSUER".to_string(), iss.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "challenge-aud".to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let a2a = triton.rest_url("/a2a");
    let mcp = triton.rest_url("/mcp");
    let send = json!({ "jsonrpc": "2.0", "id": 3, "method": "message/send", "params": {
        "message": { "kind": "message", "role": "user", "messageId": "m1",
                     "parts": [{ "kind": "text", "text": "Which agents?" }] } } });

    // A2A 0.3 JSON-RPC, no credential: the bare challenge.
    let (status, challenge, body) = post(&a2a, None, send.clone()).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(challenge.as_deref(), Some(r#"Bearer realm="triton""#));
    assert_eq!(body["error"]["code"], -32600, "{body}");

    // A credential that does not verify: `invalid_token`.
    let (status, challenge, _) = post(&a2a, Some("not-a-token"), send.clone()).await;
    assert_eq!(status, 401);
    assert_eq!(
        challenge.as_deref(),
        Some(r#"Bearer realm="triton", error="invalid_token""#)
    );

    // The 1.0 HTTP+JSON binding.
    let resp = reqwest::Client::new()
        .post(triton.rest_url("/a2a/v1/message:send"))
        .header("A2A-Version", "1.0")
        .json(
            &json!({ "message": { "role": "ROLE_USER", "messageId": "m2",
                                    "parts": [{ "text": "Which agents?" }] } }),
        )
        .send()
        .await
        .expect("POST rest");
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some(r#"Bearer realm="triton""#)
    );

    // MCP initialize and tools/list: HTTP 401 with the challenge; the
    // JSON-RPC body keeps the auth code and the request id.
    for (id, method) in [(1, "initialize"), (2, "tools/list")] {
        let (status, challenge, body) = post(
            &mcp,
            None,
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": { "name": "challenge-test", "version": "1" } } }),
        )
        .await;
        assert_eq!(status, 401, "{method}: {body}");
        assert_eq!(
            challenge.as_deref(),
            Some(r#"Bearer realm="triton""#),
            "{method}"
        );
        assert_eq!(body["error"]["code"], -32001, "{method}: {body}");
        assert_eq!(body["id"], id, "{method}: {body}");
    }
    let (status, challenge, _) = post(
        &mcp,
        Some("not-a-token"),
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(
        challenge.as_deref(),
        Some(r#"Bearer realm="triton", error="invalid_token""#)
    );

    // A verified caller is unaffected: no challenge, a 200.
    let token = iss.sign_jwt(json!({ "iss": iss.issuer_url(), "aud": "challenge-aud",
        "sub": "alice", "tenant": "acme",
        "exp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap().as_secs() + 600 }));
    let (status, challenge, body) = post(
        &mcp,
        Some(&token),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(challenge, None);
}
