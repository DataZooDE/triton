//! The public A2A agent cards are readable cross-origin.
//!
//! Copilot Studio's maker portal fetches the card from the user's browser
//! (origin `https://copilotstudio.microsoft.com`), probing every card
//! spelling at `/a2a/.well-known/` and `/.well-known/`. Without
//! `Access-Control-Allow-Origin` the browser blocks the read ("Failed to
//! fetch") and the portal reports no card. A card is public metadata, so
//! every card GET carries `Access-Control-Allow-Origin: *` without
//! credentials, and an `OPTIONS` preflight is `204`. Chrome's Private
//! Network Access preflight (`Access-Control-Request-Private-Network:
//! true`, sent when a public page reaches a private address such as a
//! tailnet IP) is answered `Access-Control-Allow-Private-Network: true`.
//!
//! The authenticated JSON-RPC endpoints are not opened: a browser POST
//! from a foreign origin still gets no CORS grant.
//!
//! No mocks: the real binary, real HTTP.

use reqwest::Method;

use crate::a2a_bin::host;

const ORIGIN: &str = "https://copilotstudio.microsoft.com";
const CARD_PATHS: &[&str] = &[
    "/.well-known/agent-card.json",
    "/.well-known/agent.json",
    "/.well-known/agentcard.json",
    "/.well-known/agentCard.json",
    "/.well-known/agent_card.json",
    "/a2a/.well-known/agent-card.json",
    "/a2a/.well-known/agent.json",
    "/a2a/.well-known/agentcard.json",
    "/a2a/.well-known/agentCard.json",
    "/a2a/.well-known/agent_card.json",
];

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().expect("ASCII header").to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_card_get_allows_any_origin_without_credentials() {
    let h = host().await;
    let client = reqwest::Client::new();
    for path in CARD_PATHS {
        let resp = client
            .get(h.url(path))
            .header("origin", ORIGIN)
            .send()
            .await
            .expect("GET card");
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(
            header(&resp, "access-control-allow-origin").as_deref(),
            Some("*"),
            "{path}"
        );
        assert_eq!(
            header(&resp, "access-control-allow-credentials"),
            None,
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_preflight_is_204_and_answers_private_network_access() {
    let h = host().await;
    let client = reqwest::Client::new();
    for path in CARD_PATHS {
        let resp = client
            .request(Method::OPTIONS, h.url(path))
            .header("origin", ORIGIN)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "accept, content-type")
            .header("access-control-request-private-network", "true")
            .send()
            .await
            .expect("OPTIONS card");
        assert_eq!(resp.status(), 204, "{path}");
        assert_eq!(
            header(&resp, "access-control-allow-origin").as_deref(),
            Some("*"),
            "{path}"
        );
        let methods = header(&resp, "access-control-allow-methods").unwrap_or_default();
        assert!(methods.contains("GET"), "{path}: {methods:?}");
        let headers = header(&resp, "access-control-allow-headers").unwrap_or_default();
        assert!(headers.contains("content-type"), "{path}: {headers:?}");
        assert_eq!(
            header(&resp, "access-control-allow-private-network").as_deref(),
            Some("true"),
            "{path}"
        );
    }

    // Without the PNA request header, no PNA grant.
    let plain = client
        .request(Method::OPTIONS, h.url("/a2a/.well-known/agent-card.json"))
        .header("origin", ORIGIN)
        .header("access-control-request-method", "GET")
        .send()
        .await
        .expect("OPTIONS card");
    assert_eq!(plain.status(), 204);
    assert_eq!(header(&plain, "access-control-allow-private-network"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_authenticated_endpoint_gets_no_cors_grant() {
    let h = host().await;
    let resp = reqwest::Client::new()
        .post(h.rpc_url())
        .header("origin", ORIGIN)
        .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "message/send" }))
        .send()
        .await
        .expect("POST a2a");
    assert_eq!(header(&resp, "access-control-allow-origin"), None);
    let preflight = reqwest::Client::new()
        .request(Method::OPTIONS, h.rpc_url())
        .header("origin", ORIGIN)
        .header("access-control-request-method", "POST")
        .send()
        .await
        .expect("OPTIONS a2a");
    assert_eq!(header(&preflight, "access-control-allow-origin"), None);
}
