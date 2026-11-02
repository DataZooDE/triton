//! MCP protocol eras on a customer path (`/mcp/<slug>`).
//!
//! MCP 2026-07-28 replaced the `initialize` handshake with a stateless
//! `server/discover` probe and per-request `_meta`
//! (`io.modelcontextprotocol/protocolVersion`, …). A client that speaks
//! both eras probes with `server/discover` first; a server that cannot
//! answer it in a way the client recognises leaves the connector with no
//! tools. Triton answers both eras on the same endpoint:
//!
//! * `server/discover` returns a `DiscoverResult` (supported versions,
//!   capabilities, server identity in `_meta`, cache hints).
//! * A request carrying the 2026-07-28 envelope is served statelessly and
//!   its result carries `resultType`; list and resource-read results carry
//!   `ttlMs` and `cacheScope` (validated against the official schemas in
//!   `mcp_schema.rs`).
//! * A notification (no `id`) is accepted with `202 Accepted` and no body
//!   (Streamable HTTP, "Sending Messages" rule 5).
//! * An unknown method is JSON-RPC `-32601`; on a 2026-07-28 request the
//!   HTTP status is `404 Not Found` (Streamable HTTP, Protocol Version
//!   Header).
//! * A 2026-07-28-style request naming a version Triton does not implement
//!   is `400` with `UnsupportedProtocolVersionError` (`-32022`).
//! * The legacy `initialize` handshake keeps working, including at
//!   `2025-11-25`, the last handshake-era revision.
//!
//! No mocks: the real binary, a `TestIssuer` playing the Entra authority,
//! and a `FakeAgent` upstream.

use std::time::Duration;

use serde_json::{Value, json};

use crate::mcp_protected_resource::{TID_ACME, entra_token, rig};

const MODERN: &str = "2026-07-28";

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientInfo": { "name": "era-test", "version": "1.0.0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

struct Reply {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "body is not JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// POST one JSON-RPC message the way a Streamable HTTP client does. `version`
/// is the `MCP-Protocol-Version` header; the `Mcp-Method` header mirrors the
/// body's `method`.
async fn send(url: &str, token: &str, version: Option<&str>, body: Value) -> Reply {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(token)
        .json(&body);
    if let Some(v) = version {
        req = req.header("mcp-protocol-version", v);
    }
    if let Some(m) = body["method"].as_str() {
        req = req.header("mcp-method", m);
    }
    let resp = req.send().await.expect("POST mcp");
    Reply {
        status: resp.status(),
        body: resp.bytes().await.expect("read body").to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_discover_returns_a_discover_result() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let url = r.triton.rest_url("/mcp/acme");
    let reply = send(
        &url,
        &token,
        Some(MODERN),
        json!({ "jsonrpc": "2.0", "id": "discover-1", "method": "server/discover",
                "params": { "_meta": modern_meta() } }),
    )
    .await;
    let body = reply.json();
    assert_eq!(reply.status, 200, "{body}");
    assert_eq!(body["id"], "discover-1");
    let result = &body["result"];
    assert_eq!(result["resultType"], "complete", "{body}");
    let versions: Vec<&str> = result["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("supportedVersions is an array: {body}"))
        .iter()
        .map(|v| v.as_str().expect("version string"))
        .collect();
    assert!(versions.contains(&MODERN), "{versions:?}");
    assert!(versions.contains(&"2025-06-18"), "{versions:?}");
    assert!(result["capabilities"]["tools"].is_object(), "{body}");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "triton"
    );
    assert!(result["ttlMs"].is_u64(), "{body}");
    assert_eq!(result["cacheScope"], "private", "{body}");

    // Also without any `_meta` (the probe carries no required params).
    let bare = send(
        &url,
        &token,
        None,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "server/discover" }),
    )
    .await;
    assert_eq!(bare.status, 200);
    assert_eq!(bare.json()["result"]["resultType"], "complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_modern_request_lists_tools_without_initialize() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let reply = send(
        &r.triton.rest_url("/mcp/acme"),
        &token,
        Some(MODERN),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list",
                "params": { "_meta": modern_meta() } }),
    )
    .await;
    let body = reply.json();
    assert_eq!(reply.status, 200, "{body}");
    let result = &body["result"];
    assert!(
        result["tools"].as_array().is_some_and(|t| !t.is_empty()),
        "{body}"
    );
    assert_eq!(result["resultType"], "complete");
    assert!(result["ttlMs"].is_u64(), "{body}");
    assert_eq!(result["cacheScope"], "private");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_are_accepted_with_202_and_no_body() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let url = r.triton.rest_url("/mcp/acme");
    for method in ["notifications/initialized", "notifications/cancelled"] {
        let reply = send(
            &url,
            &token,
            Some("2025-06-18"),
            json!({ "jsonrpc": "2.0", "method": method, "params": {} }),
        )
        .await;
        assert_eq!(reply.status, 202, "{method}");
        assert!(reply.body.is_empty(), "{method}: {:?}", reply.body);
    }
    // A notification is not a rejected call: no `rejected` audit line.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let rejected: Vec<String> = r
        .triton
        .stdout_snapshot()
        .into_iter()
        .filter(|l| l.contains("\"rejected\"") && l.contains("notifications/"))
        .collect();
    assert!(rejected.is_empty(), "{rejected:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_method_is_32601_and_404_on_a_modern_request() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let url = r.triton.rest_url("/mcp/acme");
    let modern = send(
        &url,
        &token,
        Some(MODERN),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "prompts/nope",
                "params": { "_meta": modern_meta() } }),
    )
    .await;
    assert_eq!(modern.status, 404);
    let body = modern.json();
    assert_eq!(body["error"]["code"], -32601, "{body}");
    assert_eq!(body["id"], 4);

    // Handshake era: the JSON-RPC error in a 200, as before.
    let legacy = send(
        &url,
        &token,
        Some("2025-06-18"),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "prompts/nope", "params": {} }),
    )
    .await;
    assert_eq!(legacy.status, 200);
    assert_eq!(legacy.json()["error"]["code"], -32601);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_modern_version_is_refused_with_the_supported_list() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let mut meta = modern_meta();
    meta["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let reply = send(
        &r.triton.rest_url("/mcp/acme"),
        &token,
        Some("2099-01-01"),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list",
                "params": { "_meta": meta } }),
    )
    .await;
    assert_eq!(reply.status, 400);
    let body = reply.json();
    assert_eq!(body["error"]["code"], -32022, "{body}");
    assert_eq!(body["error"]["data"]["requested"], "2099-01-01");
    assert!(
        body["error"]["data"]["supported"]
            .as_array()
            .is_some_and(|s| s.iter().any(|v| v == MODERN)),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handshake_still_works_up_to_2025_11_25() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let reply = send(
        &r.triton.rest_url("/mcp/acme"),
        &token,
        None,
        json!({ "jsonrpc": "2.0", "id": 8, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "clientInfo": { "name": "era-test", "version": "1" } } }),
    )
    .await;
    let body = reply.json();
    assert_eq!(reply.status, 200, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");
    assert_eq!(body["result"]["serverInfo"]["name"], "triton");
}

/// Streamable HTTP: the endpoint takes POST only. Triton opens no
/// standalone SSE stream, so GET (2025-03-26..2025-11-25: "return
/// `text/event-stream` or 405") and DELETE (session termination; Triton
/// mints no sessions) answer `405 Method Not Allowed` with `Allow: POST`
/// (RFC 9110 §15.5.6), with or without a token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_and_delete_on_the_endpoint_are_405_with_allow_post() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let url = r.triton.rest_url("/mcp/acme");
    let client = reqwest::Client::new();
    for (method, bearer) in [
        (reqwest::Method::GET, None),
        (reqwest::Method::GET, Some(token.as_str())),
        (reqwest::Method::DELETE, Some(token.as_str())),
    ] {
        let mut req = client
            .request(method.clone(), &url)
            .header("accept", "text/event-stream");
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.expect("request");
        assert_eq!(resp.status(), 405, "{method} token={}", bearer.is_some());
        let allow = resp
            .headers()
            .get("allow")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(allow.contains("POST"), "{method}: Allow={allow:?}");
    }
}

/// An unauthenticated POST is refused with the OAuth challenge whatever
/// its body: a connector probing the endpoint with an empty POST must get
/// the `401` + `WWW-Authenticate` that starts its sign-in, not a parse
/// error. Once authenticated, an empty or malformed body is a JSON-RPC
/// parse error (`-32700`, id null) with `400 Bad Request`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_post_is_401_without_a_token_and_400_parse_error_with_one() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let url = r.triton.rest_url("/mcp/acme");
    let client = reqwest::Client::new();

    let anon = client
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .send()
        .await
        .expect("POST");
    assert_eq!(anon.status(), 401);
    let challenge = anon
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(challenge.contains("resource_metadata"), "{challenge:?}");

    let authed = client
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST");
    assert_eq!(authed.status(), 400);
    let body: Value = authed.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], -32700, "{body}");
    assert!(body["id"].is_null(), "{body}");
}
