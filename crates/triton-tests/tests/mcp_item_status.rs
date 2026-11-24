//! The status of a filed item on the MCP Apps answer card, against the real
//! binary.
//!
//! A tool that files something names, in its result `_meta.item_status`, a
//! read-only status tool and its arguments. Pinned here:
//!
//! * a card host gets `_meta["triton/status"]` with the state, the label, a
//!   status-poll token and the schedule; a plain MCP client gets nothing
//!   (and never the upstream's `item_status`);
//! * the card's poll (`triton_ui_event`, event `triton.status`) calls the
//!   status tool with the signed arguments, marked `status_poll`, and the
//!   answer carries the next state: a fresh token while pending, none once
//!   terminal;
//! * a status poll's answer is the status only (no card, no controls, no
//!   upstream keys);
//! * a status tool that may not be called in place (not declared in-card
//!   capable, or executed by another endpoint than the filing tool) shows
//!   the state without a token;
//! * the poll token is refused for another principal and for any other
//!   event.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_core::a2ui::{in_card, item_status, mcp_app};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const ENTRA_AUD: &str = "api://triton-mcp";
const TID: &str = "11111111-2222-3333-4444-abcdefabcdef";
const ORIGIN: &str = "https://agents.example.test";
const KEY: &str = "item-status-integration-key-01234567";
const UI: &str = "io.modelcontextprotocol/ui";
const ALICE: &str = "00000000-0000-0000-0000-0000000000aa";
const BOB: &str = "00000000-0000-0000-0000-0000000000bb";

struct Rig {
    triton: TritonProcess,
    entra: TestIssuer,
    /// One endpoint: `file_item` (files), `item_status` (its status tool,
    /// declared in-card capable) and `item_status_plain` (not declared). Its
    /// answer is set per step ([`FakeAgent::set_response`]).
    agent: FakeAgent,
    /// Another endpoint with an in-card capable read-only `foreign_status`.
    other: FakeAgent,
}

fn filed(status_tool: &str) -> Value {
    json!({
        "text": "Filed request r-1.",
        "surface": { "components": [ { "kind": "text", "value": "Filed request r-1." } ] },
        "_meta": { "item_status": {
            "tool": status_tool, "args": { "request_id": "r-1" },
            "state": "pending", "terminal": false, "label": "Waiting for review"
        } }
    })
}

fn status_answer(state: &str, terminal: bool, label: &str) -> Value {
    json!({
        "text": label,
        "surface": { "components": [ { "kind": "text", "value": label } ] },
        "_meta": { "item_status": {
            "tool": "item_status", "args": { "request_id": "r-1" },
            "state": state, "terminal": terminal, "label": label
        }, "chart": { "kind": "bar" } }
    })
}

async fn rig() -> Rig {
    let entra = TestIssuer::start().await;
    let agent = FakeAgent::start_returning(filed("item_status")).await;
    agent.set_tools_list(json!({ "tools": [
        { "name": "file_item", "annotations": { "readOnlyHint": false } },
        { "name": "item_status", "annotations": { "readOnlyHint": true } },
        { "name": "item_status_plain", "annotations": { "readOnlyHint": true } }
    ] }));
    let other = FakeAgent::start_returning(status_answer("pending", false, "Waiting")).await;
    other.set_tools_list(json!({ "tools": [
        { "name": "foreign_status", "annotations": { "readOnlyHint": true } }
    ] }));
    let manifest = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "item-status-{}.yaml",
        triton_tests::free_tcp_port()
    ));
    std::fs::write(
        &manifest,
        "version: \"0.2\"\nadapters: {}\ntools:\n  item_status:\n    in_card: true\n  \
         foreign_status:\n    in_card: true\n",
    )
    .unwrap();
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_ENTRA_AUDIENCE".to_string(), ENTRA_AUD.to_string()),
        (
            "TRITON_ENTRA_ALLOWED_TENANTS".to_string(),
            format!("{TID}=acme"),
        ),
        ("TRITON_ENTRA_AUTHORITY".to_string(), entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL".to_string(), entra.jwks_url()),
        (
            "TRITON_ENTRA_SCOPES".to_string(),
            "api://triton-mcp/access_as_user".to_string(),
        ),
        ("TRITON_MCP_TENANTS".to_string(), format!("acme=acme:{TID}")),
        ("TRITON_MCP_PUBLIC_ORIGIN".to_string(), ORIGIN.to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "file_item={a},item_status={a},item_status_plain={a},foreign_status={o}",
                a = agent.host_port(),
                o = other.host_port()
            ),
        ),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        ("TRITON_A2UI_ACTION_KEY".to_string(), KEY.to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            manifest.display().to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    Rig {
        triton,
        entra,
        agent,
        other,
    }
}

fn bearer(entra: &TestIssuer, oid: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD,
        "sub": format!("pairwise-{oid}"),
        "oid": oid,
        "tid": TID,
        "preferred_username": format!("{oid}@acme.example"),
        "exp": now + 600,
        "iat": now,
        "scp": "access_as_user",
    }))
}

async fn post(
    r: &Rig,
    user: &str,
    session: Option<&str>,
    method: &str,
    params: Value,
) -> (Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(bearer(&r.entra, user))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(s) = session {
        req = req.header("mcp-session-id", s);
    }
    let resp = req.send().await.expect("POST /mcp/acme");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "{method}");
    let session = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    let body: Value = resp.json().await.expect("json");
    assert!(body.get("error").is_none(), "{method}: {body}");
    (session, body["result"].clone())
}

async fn card_session(r: &Rig, user: &str) -> String {
    post(
        r,
        user,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "extensions": { UI: { "mimeTypes": [mcp_app::MIME_TYPE] } } },
            "clientInfo": { "name": "item-status-test", "version": "1" }
        }),
    )
    .await
    .0
    .expect("a card host gets a session")
}

async fn call(r: &Rig, user: &str, session: Option<&str>, tool: &str, args: Value) -> Value {
    post(
        r,
        user,
        session,
        "tools/call",
        json!({ "name": tool, "arguments": args }),
    )
    .await
    .1
}

fn poll(token: &str, name: &str) -> Value {
    json!({ "surfaceId": "triton-card", "name": name, "sourceComponentId": "",
            "context": {}, "token": token })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_host_polls_a_filed_item_until_its_status_is_terminal() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let res = call(
        &r,
        ALICE,
        Some(&session),
        "file_item",
        json!({ "text": "x" }),
    )
    .await;
    let st = &res["_meta"][item_status::META_KEY];
    assert_eq!(st["state"], "pending", "{res}");
    assert_eq!(st["terminal"], false);
    assert_eq!(st["label"], "Waiting for review");
    assert_eq!(st["poll"]["after_ms"], item_status::POLL_AFTER_MS);
    let token = st["token"]
        .as_str()
        .expect("a pending status carries a token")
        .to_string();
    // The upstream's own key never reaches the client.
    assert!(res["_meta"].get("item_status").is_none(), "{res}");
    assert!(!res.to_string().contains("\"item_status\""), "{res}");

    // Still pending: the poll answers with the state and a fresh token, and
    // nothing else (no card, no controls, none of the upstream's keys).
    r.agent
        .set_response(status_answer("pending", false, "Still waiting"));
    let mut seen_before = r.agent.bodies_seen().len();
    let res = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        poll(&token, in_card::STATUS_EVENT),
    )
    .await;
    assert_eq!(res["isError"], false, "{res}");
    let st = &res["_meta"][item_status::META_KEY];
    assert_eq!(st["state"], "pending", "{res}");
    assert_eq!(res["content"][0]["text"], "Still waiting");
    let token = st["token"]
        .as_str()
        .expect("still pending: a fresh token")
        .to_string();
    for key in [mcp_app::META_KEY, in_card::META_KEY, "chart", "item_status"] {
        assert!(res["_meta"].get(key).is_none(), "{key}: {res}");
    }
    let bodies = r.agent.bodies_seen();
    assert_eq!(bodies.len(), seen_before + 1);
    let sent = &bodies[bodies.len() - 1];
    assert_eq!(sent["request_id"], "r-1", "{sent}");
    assert_eq!(sent["_triton"]["client"]["status_poll"], true, "{sent}");
    assert_eq!(
        r.agent.tools_seen().last().unwrap().as_deref(),
        Some("item_status")
    );

    // Decided: the answer is terminal and carries no token.
    r.agent
        .set_response(status_answer("accepted", true, "Accepted by Jane Doe"));
    seen_before = r.agent.bodies_seen().len();
    let res = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        poll(&token, in_card::STATUS_EVENT),
    )
    .await;
    let st = &res["_meta"][item_status::META_KEY];
    assert_eq!(st["state"], "accepted", "{res}");
    assert_eq!(st["terminal"], true);
    assert_eq!(st["label"], "Accepted by Jane Doe");
    assert!(
        st.get("token").is_none() && st.get("poll").is_none(),
        "{st}"
    );
    assert_eq!(r.agent.bodies_seen().len(), seen_before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_client_gets_no_status_and_an_unpollable_tool_no_token() {
    let r = rig().await;
    // A client that does not render the card: no status at all.
    let res = call(&r, ALICE, None, "file_item", json!({})).await;
    assert!(res["_meta"].get(item_status::META_KEY).is_none(), "{res}");
    assert!(res["_meta"].get("item_status").is_none(), "{res}");

    let session = card_session(&r, ALICE).await;
    // A status tool the operator did not declare in-card capable, and one
    // of another endpoint: the state is shown, without a token.
    for status_tool in ["item_status_plain", "foreign_status"] {
        r.agent.set_response(filed(status_tool));
        let res = call(&r, ALICE, Some(&session), "file_item", json!({})).await;
        let st = &res["_meta"][item_status::META_KEY];
        assert_eq!(st["state"], "pending", "{status_tool}: {res}");
        assert_eq!(st["label"], "Waiting for review");
        assert!(
            st.get("token").is_none() && st.get("poll").is_none(),
            "{status_tool}: {st}"
        );
    }
    assert_eq!(r.other.hits(), 0, "the other endpoint was never called");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poll_token_is_for_its_principal_and_the_status_event_only() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let res = call(&r, ALICE, Some(&session), "file_item", json!({})).await;
    let token = res["_meta"][item_status::META_KEY]["token"]
        .as_str()
        .expect("token")
        .to_string();
    let before = r.agent.bodies_seen().len();

    let bob = card_session(&r, BOB).await;
    let res = call(
        &r,
        BOB,
        Some(&bob),
        in_card::TOOL_NAME,
        poll(&token, in_card::STATUS_EVENT),
    )
    .await;
    assert_eq!(res["isError"], true, "another principal: {res}");
    let res = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        poll(&token, in_card::EVENT),
    )
    .await;
    assert_eq!(res["isError"], true, "another event: {res}");
    assert_eq!(
        r.agent.bodies_seen().len(),
        before,
        "nothing reached the status tool"
    );
}
