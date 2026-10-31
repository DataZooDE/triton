//! Every Entra token must carry the delegated scope, on every face
//! (lead decision 2026-10-31, ADR-22).
//!
//! An app-only (client-credentials) token from an allow-listed customer
//! tenant has the right `iss`, `tid` and `aud`, carries `roles` and no
//! `scp`, and has no user behind it. A user token for another permission of
//! the same API is not this one either. Both are refused with 403 and
//! `WWW-Authenticate: Bearer realm="triton", error="insufficient_scope",
//! scope="<the Entra API scope>"` (RFC 6750 §3.1) on REST tools, MCP, the
//! Triton-shaped A2A route and spec A2A 0.3 / 1.0 (JSON-RPC and HTTP+JSON),
//! and nothing is dispatched. Machine-to-machine access is not offered.
//! Google access tokens and the Teams bot identity are not Entra access
//! tokens and are unaffected (`inbound_issuers.rs`).
//!
//! No mocks: the real binary on a single port, a `TestIssuer` as the Entra
//! authority, and a `FakeAgent` upstream that records every dispatch.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const ENTRA_AUD: &str = "api://triton-copilot";
const SCOPE: &str = "api://triton-copilot/access_as_user";
const TID: &str = "11111111-2222-3333-4444-abcdefabcdef";
const PUBLIC: &str = "https://agents.example.test/a2a";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Rig {
    triton: TritonProcess,
    agent: FakeAgent,
    entra: TestIssuer,
}

async fn rig(extra: &[(&str, &str)]) -> Rig {
    let agent = FakeAgent::start_echoing().await;
    let entra = TestIssuer::start().await;
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        ("TRITON_ENTRA_AUDIENCE".to_string(), ENTRA_AUD.to_string()),
        (
            "TRITON_ENTRA_ALLOWED_TENANTS".to_string(),
            format!("{TID}=acme"),
        ),
        ("TRITON_ENTRA_AUTHORITY".to_string(), entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL".to_string(), entra.jwks_url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
        ("TRITON_A2A_PUBLIC_URL".to_string(), PUBLIC.to_string()),
        (
            "TRITON_A2A_DEFAULT_TOOL".to_string(),
            "assistant".to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    Rig {
        triton,
        agent,
        entra,
    }
}

fn user_token(entra: &TestIssuer, scp: &str) -> String {
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD, "sub": "pairwise-bob", "tid": TID,
        "oid": "00000000-0000-0000-0000-0000000000bb",
        "preferred_username": "bob@acme.example",
        "scp": scp, "exp": now() + 600, "iat": now(),
    }))
}

fn app_only_token(entra: &TestIssuer) -> String {
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD, "sub": "service-principal", "tid": TID,
        "oid": "00000000-0000-0000-0000-0000000000cc",
        "idtyp": "app", "roles": ["Tools.Execute.All"],
        "exp": now() + 600, "iat": now(),
    }))
}

/// One request per face: (label, path, A2A-Version, body).
fn faces() -> Vec<(&'static str, &'static str, Option<&'static str>, Value)> {
    vec![
        (
            "REST tools",
            "/v1/tools/assistant",
            None,
            json!({ "message": "hi" }),
        ),
        (
            "MCP",
            "/mcp",
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": "assistant", "arguments": { "message": "hi" } } }),
        ),
        (
            "A2A Triton-shaped",
            "/a2a/message:send",
            None,
            json!({ "parts": [{ "data": { "tool": "assistant", "args": { "message": "hi" } } }] }),
        ),
        (
            "A2A 0.3 JSON-RPC",
            "/a2a",
            None,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "message/send", "params": {
                "message": { "kind": "message", "role": "user", "messageId": "m03",
                             "parts": [{ "kind": "text", "text": "hi" }] } } }),
        ),
        (
            "A2A 1.0 JSON-RPC",
            "/a2a",
            Some("1.0"),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "SendMessage", "params": {
                "message": { "role": "ROLE_USER", "messageId": "m10",
                             "parts": [{ "text": "hi" }] } } }),
        ),
        (
            "A2A 1.0 HTTP+JSON",
            "/a2a/v1/message:send",
            Some("1.0"),
            json!({ "message": { "role": "ROLE_USER", "messageId": "m10r",
                                 "parts": [{ "text": "hi" }] } }),
        ),
    ]
}

async fn send(
    triton: &TritonProcess,
    path: &str,
    version: Option<&str>,
    token: &str,
    body: &Value,
) -> (reqwest::StatusCode, Option<String>, String) {
    let mut req = reqwest::Client::new()
        .post(triton.rest_url(path))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(token)
        .json(body);
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    let resp = req.send().await.expect("POST");
    let status = resp.status();
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string());
    (status, challenge, resp.text().await.unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entra_token_without_the_delegated_scope_is_refused_on_every_face() {
    let r = rig(&[]).await;
    let expected = format!(r#"Bearer realm="triton", error="insufficient_scope", scope="{SCOPE}""#);
    for (label, token) in [
        ("app-only", app_only_token(&r.entra)),
        ("other scope", user_token(&r.entra, "User.Read")),
    ] {
        for (face, path, version, body) in faces() {
            let (status, challenge, text) = send(&r.triton, path, version, &token, &body).await;
            assert_eq!(
                status,
                reqwest::StatusCode::FORBIDDEN,
                "{label} on {face}: {text}"
            );
            assert_eq!(
                challenge.as_deref(),
                Some(expected.as_str()),
                "{label} on {face}"
            );
        }
    }
    assert!(r.agent.bearers_seen().is_empty(), "nothing was dispatched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entra_user_token_with_the_delegated_scope_passes_on_every_face() {
    let r = rig(&[]).await;
    // `scp` is space-delimited; the scope may sit beside others.
    let token = user_token(&r.entra, "User.Read access_as_user");
    let mut dispatched = 0;
    for (face, path, version, body) in faces() {
        let (status, challenge, text) = send(&r.triton, path, version, &token, &body).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{face}: {text}");
        assert_eq!(challenge, None, "{face}");
        dispatched += 1;
        assert_eq!(
            r.agent.bearers_seen().len(),
            dispatched,
            "{face} reached the upstream"
        );
    }
}

/// The required scope is the configured one (`TRITON_ENTRA_SCOPES`), by its
/// last path segment, which is what Entra puts in `scp`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_required_scope_follows_the_configured_entra_scope() {
    let r = rig(&[("TRITON_ENTRA_SCOPES", "api://triton-copilot/tools.execute")]).await;
    let (face, path, version, body) = faces().remove(0);
    let (status, challenge, _) = send(
        &r.triton,
        path,
        version,
        &user_token(&r.entra, "access_as_user"),
        &body,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{face}");
    assert_eq!(
        challenge.as_deref(),
        Some(
            r#"Bearer realm="triton", error="insufficient_scope", scope="api://triton-copilot/tools.execute""#
        )
    );
    let (status, _, text) = send(
        &r.triton,
        path,
        version,
        &user_token(&r.entra, "tools.execute"),
        &body,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{text}");
}
