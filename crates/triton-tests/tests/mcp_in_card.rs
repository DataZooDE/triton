//! In-card actions on the MCP Apps answer card (#369), against the real
//! binary.
//!
//! A card control that asks for the same answer with other arguments (a
//! page, a filter chip, a refresh) calls the app-only tool
//! `triton_ui_event` instead of sending a new user turn. Triton re-runs the
//! producing tool and the result replaces the card. Pinned here:
//!
//! * `tools/list` lists the app-only tool (hidden from the model,
//!   `_meta.ui.visibility: ["app"]`) only to a client that renders the card;
//! * a card host gets signed in-card controls, every other client the
//!   fallback messages; a tool the operator did not declare in-card capable
//!   keeps the fallbacks too;
//! * a page turn and a filter chip update the card (new A2UI messages)
//!   through `tools/call`, the upstream sees `_triton.client.in_card`, and
//!   the dispatcher writes one audit line with the in-card part;
//! * a producer surface's `triton.in_card:<name>` event reaches the upstream
//!   under `_triton.client.a2ui_events`;
//! * refused: a tool that is not read-only, another principal's token, an
//!   expired or tampered token; and a burst over the per-principal limit.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use triton_core::a2ui::ge::ActionSigner;
use triton_core::a2ui::in_card::{self, Holder, Scope};
use triton_core::a2ui::{basic, mcp_app};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const ENTRA_AUD: &str = "api://triton-mcp";
const TID: &str = "11111111-2222-3333-4444-abcdefabcdef";
const ORIGIN: &str = "https://agents.example.test";
const KEY: &str = "in-card-integration-key-0123456789";
const UI: &str = "io.modelcontextprotocol/ui";

struct Rig {
    triton: TritonProcess,
    entra: TestIssuer,
    agent: FakeAgent,
    /// Advertises the cards agent's tool names with the opposite
    /// annotations; routed only `decoy_tool`.
    decoy: FakeAgent,
    /// Echoes (no card); routed `plain_reads`, declared in-card capable.
    echo: FakeAgent,
    /// Two producer surfaces with one in-card event each; `two_panels`.
    panels: FakeAgent,
}

/// The body of `two_panels`: surface 0 fires `triton.in_card:refresh`,
/// surface 1 `triton.in_card:next`.
fn two_panels() -> Value {
    let panel = |event: &str| {
        json!({
            "catalogId": "https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json",
            "components": [
                { "id": "root", "component": "Column", "children": ["b"] },
                { "id": "bl", "component": "Text", "text": event },
                { "id": "b", "component": "Button", "child": "bl",
                  "action": { "event": { "name": event } } }
            ]
        })
    };
    json!({
        "text": "Two panels",
        "surface": {
            "components": [ { "kind": "text", "value": "Two panels" } ],
            "a2ui": [ panel("triton.in_card:refresh"), panel("triton.in_card:next") ]
        }
    })
}

async fn rig() -> Rig {
    let entra = TestIssuer::start().await;
    let agent = FakeAgent::start_cards().await;
    let decoy = FakeAgent::start_echoing().await;
    decoy.set_tools_list(json!({ "tools": [
        { "name": "file_change", "annotations": { "readOnlyHint": true } },
        { "name": "list_planned", "annotations": { "readOnlyHint": false } },
        { "name": "decoy_tool", "annotations": { "readOnlyHint": true } }
    ] }));
    let echo = FakeAgent::start_echoing().await;
    echo.set_tools_list(json!({ "tools": [
        { "name": "plain_reads", "annotations": { "readOnlyHint": true } }
    ] }));
    let panels = FakeAgent::start_returning(two_panels()).await;
    panels.set_tools_list(json!({ "tools": [
        { "name": "two_panels", "annotations": { "readOnlyHint": true } }
    ] }));
    let manifest = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("in-card-{}.yaml", triton_tests::free_tcp_port()));
    std::fs::write(
        &manifest,
        "version: \"0.2\"\nadapters: {}\ntools:\n  list_planned:\n    in_card: true\n  \
         file_change:\n    in_card: true\n  plain_reads:\n    in_card: true\n  \
         two_panels:\n    in_card: true\n  decoy_tool:\n    in_card: true\n",
    )
    .unwrap();
    let hp = agent.host_port();
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
        // `list_plain` is the same agent, not declared in-card capable.
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "list_planned={hp},file_change={hp},list_plain={hp},decoy_tool={},\
                 plain_reads={},two_panels={}",
                decoy.host_port(),
                echo.host_port(),
                panels.host_port()
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
        decoy,
        echo,
        panels,
    }
}

/// A bearer for user `oid` of the customer tenant.
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

const ALICE: &str = "00000000-0000-0000-0000-0000000000aa";
const BOB: &str = "00000000-0000-0000-0000-0000000000bb";

/// One JSON-RPC exchange; `session` makes it a card host's (handshake era).
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

/// A card host's session (declares MCP Apps in `initialize`).
async fn card_session(r: &Rig, user: &str) -> String {
    post(
        r,
        user,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "extensions": { UI: { "mimeTypes": [mcp_app::MIME_TYPE] } } },
            "clientInfo": { "name": "in-card-test", "version": "1" }
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

/// Every event Triton's card fires, as `(name, context)`.
fn card_events(res: &Value) -> Vec<(String, Value)> {
    res["_meta"][mcp_app::META_KEY]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["updateComponents"]["surfaceId"] == basic::SURFACE_ID)
        .flat_map(|m| {
            m["updateComponents"]["components"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|c| c["action"].get("event").cloned())
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["context"].clone(),
            )
        })
        .collect()
}

/// The in-card token of the control whose fallback is `fallback`.
fn token_for(res: &Value, fallback: &str) -> String {
    card_events(res)
        .into_iter()
        .find(|(n, c)| n == in_card::EVENT && c["fallback"] == fallback)
        .unwrap_or_else(|| panic!("no in-card control `{fallback}`: {:#?}", card_events(res)))
        .1["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Every shown text of the widget's messages.
fn texts(res: &Value) -> String {
    res["_meta"][mcp_app::META_KEY]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| {
            m["updateComponents"]["components"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|c| c["text"].as_str().map(String::from))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn ui_event(token: &str, name: &str, surface: &str, context: Value) -> Value {
    json!({ "surfaceId": surface, "name": name, "sourceComponentId": "c1",
            "context": context, "token": token })
}

/// The audit lines of `trace_id` on Triton's stdout.
fn audit_lines(r: &Rig, trace_id: &str) -> Vec<Value> {
    r.triton
        .stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "audit" && v["trace_id"] == trace_id)
        .collect()
}

async fn wait_audit(r: &Rig, trace_id: &str) -> Vec<Value> {
    for _ in 0..50 {
        let l = audit_lines(r, trace_id);
        if !l.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return audit_lines(r, trace_id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no audit line for {trace_id}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_app_only_tool_is_listed_to_card_hosts_only() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let (_, listed) = post(&r, ALICE, Some(&session), "tools/list", json!({})).await;
    let tool = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == in_card::TOOL_NAME)
        .cloned()
        .expect("a card host sees the app-only tool");
    assert_eq!(tool["_meta"]["ui"]["visibility"], json!(["app"]));
    assert_eq!(tool["_meta"]["openai/widgetAccessible"], true);
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert_eq!(tool["inputSchema"]["type"], "object");

    // A plain MCP client (no extension): not listed.
    let (_, plain) = post(&r, ALICE, None, "tools/list", json!({})).await;
    assert!(
        plain["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["name"] != in_card::TOOL_NAME),
        "{plain}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_controls_are_signed_for_card_hosts_and_fallbacks_elsewhere() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let res = call(
        &r,
        ALICE,
        Some(&session),
        "list_planned",
        json!({ "q": "planned" }),
    )
    .await;
    let ev = card_events(&res);
    assert!(ev.iter().any(|(n, c)| n == in_card::EVENT
        && c["fallback"] == "Show page 2 of the planned agents"
        && c["token"].is_string()));
    assert!(
        ev.iter().any(|(n, c)| n == in_card::EVENT
            && c["fallback"] == "Show only the high-risk planned agents")
    );
    // A chip without `rerun` stays a new turn.
    assert!(
        ev.iter()
            .any(|(n, c)| n == basic::SEND_MESSAGE && c["text"] == "Show the drafts")
    );
    // The producer surface's in-card events carry the result's token.
    assert!(
        res["_meta"][in_card::META_KEY].is_string(),
        "{}",
        res["_meta"]
    );

    // Not a card host: no card at all (its data stays with clients that
    // render it), so no control and no token either.
    let plain = call(&r, ALICE, None, "list_planned", json!({})).await;
    assert!(card_events(&plain).is_empty(), "{plain}");
    assert!(
        plain["_meta"]
            .get(triton_core::a2ui::mcp_app::META_KEY)
            .is_none(),
        "{plain}"
    );
    assert!(plain["_meta"].get(in_card::META_KEY).is_none());

    // A card host, but a tool not declared in-card capable: fallbacks.
    let undeclared = call(&r, ALICE, Some(&session), "list_plain", json!({})).await;
    assert!(
        card_events(&undeclared)
            .iter()
            .all(|(n, _)| n != in_card::EVENT)
    );
    assert!(undeclared["_meta"].get(in_card::META_KEY).is_none());
}

/// A valid in-card token replayed by a request that does not declare MCP
/// Apps (here: no card session) gets the answer as any non-card client does:
/// the full text and envelope, no card messages, no fresh in-card token. The
/// card host rule is decided per request on the rerun path too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_replayed_without_declaring_mcp_apps_gets_no_card() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let page1 = call(
        &r,
        ALICE,
        Some(&session),
        "list_planned",
        json!({ "q": "planned" }),
    )
    .await;
    let next = token_for(&page1, "Show page 2 of the planned agents");
    let replay = call(
        &r,
        ALICE,
        None,
        in_card::TOOL_NAME,
        ui_event(
            &next,
            in_card::EVENT,
            basic::SURFACE_ID,
            json!({ "fallback": "x" }),
        ),
    )
    .await;
    assert_eq!(replay["isError"], false, "{replay}");
    assert!(
        replay["_meta"]
            .get(triton_core::a2ui::mcp_app::META_KEY)
            .is_none(),
        "{replay}"
    );
    assert!(replay["_meta"].get(in_card::META_KEY).is_none(), "{replay}");
    assert!(
        replay["structuredContent"]["result"].is_object(),
        "the full envelope: {replay}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_turn_and_a_filter_update_the_card_in_place() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let page1 = call(
        &r,
        ALICE,
        Some(&session),
        "list_planned",
        json!({ "q": "planned" }),
    )
    .await;
    assert!(texts(&page1).contains("Page 1 of 3"));

    // "Next": the same tool, page 2, the original arguments kept.
    let next = token_for(&page1, "Show page 2 of the planned agents");
    let page2 = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(
            &next,
            in_card::EVENT,
            basic::SURFACE_ID,
            json!({ "fallback": "x" }),
        ),
    )
    .await;
    assert_eq!(page2["isError"], false, "{page2}");
    assert!(texts(&page2).contains("Page 2 of 3"), "{}", texts(&page2));
    assert!(texts(&page2).contains("Agent 2-1"));
    // The model's text is the one-line card text (the widget passes it on
    // with ui/update-model-context).
    let line = page2["content"][0]["text"].as_str().unwrap();
    assert!(
        line.starts_with("Planned agents (Page 2 of 3): Agent 2-1, Agent 2-2"),
        "{line}"
    );
    // And no rows or paging state in the model-visible typed result: a
    // summary of the card instead (a model that reads it would page itself).
    assert_eq!(
        page2["structuredContent"]["card"]["shown_in_card"], true,
        "{page2}"
    );
    // The header's context line, as the text says it: which page is shown.
    assert_eq!(
        page2["structuredContent"]["card"]["context"], "Page 2 of 3",
        "{page2}"
    );
    assert!(
        page2["structuredContent"].get("result").is_none(),
        "{page2}"
    );
    let body = r.agent.bodies_seen().last().cloned().unwrap();
    assert_eq!(body["page"], 2);
    assert_eq!(body["q"], "planned");
    assert_eq!(body["_triton"]["client"]["in_card"], true);
    assert_eq!(
        r.agent.tools_seen().last().cloned().flatten().as_deref(),
        Some("list_planned")
    );

    // One audit line, from the dispatcher, with the in-card part.
    let trace = page2["_meta"]["trace_id"].as_str().unwrap().to_string();
    let lines = wait_audit(&r, &trace).await;
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let l = &lines[0];
    assert_eq!(l["tool"], "list_planned");
    assert_eq!(l["result"], "ok");
    assert_eq!(l["subject"], format!("pairwise-{ALICE}"));
    assert_eq!(l["in_card"]["ingress"], "mcp-app-event");
    assert_eq!(l["in_card"]["kind"], "rerun");
    assert_eq!(l["in_card"]["token"], "verified");
    assert_eq!(l["in_card"]["context_keys"], json!(["fallback"]));

    // The new card's controls chain: page 3 from page 2.
    let next = token_for(&page2, "Show page 3 of the planned agents");
    let page3 = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&next, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    assert!(texts(&page3).contains("Page 3 of 3"));

    // A filter chip.
    let high = token_for(&page1, "Show only the high-risk planned agents");
    let filtered = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&high, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    assert!(
        texts(&filtered).contains("Filter: high"),
        "{}",
        texts(&filtered)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_producer_event_reaches_the_upstream_as_an_a2ui_event() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let res = call(
        &r,
        ALICE,
        Some(&session),
        "list_planned",
        json!({ "q": "planned" }),
    )
    .await;
    assert!(texts(&res).contains("Events: 0"));
    let token = res["_meta"][in_card::META_KEY]
        .as_str()
        .unwrap()
        .to_string();
    let surface = res["_meta"][mcp_app::META_KEY]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|m| {
            m["createSurface"]["surfaceId"]
                .as_str()
                .filter(|s| *s != basic::SURFACE_ID)
        })
        .unwrap()
        .to_string();
    let refreshed = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(
            &token,
            "triton.in_card:refresh",
            &surface,
            json!({ "rev": "r1" }),
        ),
    )
    .await;
    assert_eq!(refreshed["isError"], false, "{refreshed}");
    assert!(
        texts(&refreshed).contains("Events: 1 \u{b7} in card: true"),
        "{}",
        texts(&refreshed)
    );
    let body = r.agent.bodies_seen().last().cloned().unwrap();
    let ev = &body["_triton"]["client"]["a2ui_events"][0]["action"];
    assert_eq!(ev["name"], "triton.in_card:refresh");
    assert_eq!(ev["surfaceId"], surface.as_str());
    assert_eq!(ev["context"]["rev"], "r1");
    assert_eq!(body["q"], "planned");

    // The token allows the events the answer carried, nothing else.
    let hits = r.agent.hits();
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&token, "triton.in_card:export", &surface, json!({})),
    )
    .await;
    refused(&out, "validation");
    assert_eq!(r.agent.hits(), hits);
}

fn refused(res: &Value, class: &str) {
    assert_eq!(res["isError"], true, "{res}");
    assert_eq!(res["structuredContent"]["error"], class, "{res}");
    assert!(res["_meta"].get(mcp_app::META_KEY).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_card_calls_are_refused_unless_read_only_and_signed_for_the_caller() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;

    // A token minted with the deployment's key for Alice (as Triton would).
    let signer = ActionSigner::new(triton_correlation::KeyRing::parse(KEY).unwrap());
    let sub = format!("pairwise-{ALICE}");
    let issuer = format!("{}/{TID}/v2.0", r.entra.issuer_url());
    let alice = Holder {
        tenant: "acme",
        sub: &sub,
        issuer: &issuer,
    };

    // `file_change` is declared in-card capable but annotated as writing:
    // its card gets no in-card controls (only fallbacks and no producer
    // token), and a token for it is refused before the upstream.
    let res = call(&r, ALICE, Some(&session), "file_change", json!({})).await;
    assert!(card_events(&res).iter().all(|(n, _)| n != in_card::EVENT));
    assert!(res["_meta"].get(in_card::META_KEY).is_none());
    let tok = in_card::sign(&signer, "file_change", &Scope::Rerun, &json!({}), alice).unwrap();
    let hits = r.agent.hits();
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&tok, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    refused(&out, "forbidden");
    assert_eq!(
        r.agent.hits(),
        hits,
        "a refused in-card call never reaches the upstream"
    );

    // Alice's control, used by Bob.
    let res = call(&r, ALICE, Some(&session), "list_planned", json!({})).await;
    let alices = token_for(&res, "Show page 2 of the planned agents");
    let bob_session = card_session(&r, BOB).await;
    let out = call(
        &r,
        BOB,
        Some(&bob_session),
        in_card::TOOL_NAME,
        ui_event(&alices, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    refused(&out, "auth");

    // Tampered: another tool's name cannot be swapped in.
    let mut t = alices.clone().into_bytes();
    let i = t.len() / 4;
    t[i] = if t[i] == b'x' { b'y' } else { b'x' };
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(
            &String::from_utf8(t).unwrap(),
            in_card::EVENT,
            basic::SURFACE_ID,
            json!({}),
        ),
    )
    .await;
    refused(&out, "auth");

    // Expired: minted with the deployment's key, three hours ago.
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 3 * 3600;
    let stale = in_card::sign_at(
        &signer,
        "list_planned",
        &Scope::Rerun,
        &json!({ "page": 2 }),
        alice,
        past,
    )
    .unwrap();
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&stale, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    refused(&out, "auth");
    // ... and the same, unexpired, is honoured: the key and the binding are
    // the deployment's, so only the expiry refused it.
    let fresh = in_card::sign(
        &signer,
        "list_planned",
        &Scope::Rerun,
        &json!({ "page": 2 }),
        alice,
    )
    .unwrap();
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        ui_event(&fresh, in_card::EVENT, basic::SURFACE_ID, json!({})),
    )
    .await;
    assert_eq!(out["isError"], false, "{out}");

    // No token at all.
    let out = call(
        &r,
        ALICE,
        Some(&session),
        in_card::TOOL_NAME,
        json!({ "surfaceId": basic::SURFACE_ID, "name": in_card::EVENT }),
    )
    .await;
    refused(&out, "auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_in_card_calls_is_rate_limited_per_principal() {
    let r = rig().await;
    let session = card_session(&r, BOB).await;
    let res = call(&r, BOB, Some(&session), "list_planned", json!({})).await;
    let tok = token_for(&res, "Show page 2 of the planned agents");
    // All at once: the outcome does not depend on how fast calls finish.
    let outs = futures::future::join_all((0..20).map(|_| {
        call(
            &r,
            BOB,
            Some(&session),
            in_card::TOOL_NAME,
            ui_event(&tok, in_card::EVENT, basic::SURFACE_ID, json!({})),
        )
    }))
    .await;
    let mut limited = 0;
    for out in outs {
        if out["isError"] == true {
            assert_eq!(out["structuredContent"]["error"], "ratelimit", "{out}");
            limited += 1;
        }
    }
    assert!(limited > 0, "20 calls in a burst: some are limited");
    assert!(limited < 20, "the burst allowance admits the first calls");
}

/// The card of the paged list (pages 1 and 2) as the widget gets it, with
/// stand-in tokens (`tok:<args>`), pinned for the browser harness
/// (`deploy/local-e2e/mcp-widget/run.sh`, `in-card-<page>.a2ui.json`).
#[test]
fn the_in_card_goldens_are_pinned() {
    use crate::mcp_app_widget::{golden, pretty};
    let sign = |args: &serde_json::Map<String, Value>| {
        Some(format!("tok:{}", Value::Object(args.clone())))
    };
    let schema = crate::conversation_components::a2ui_basic_validator();
    for page in [1, 2] {
        let result = triton_tests::upstream_fixture::cards_response(&json!({ "page": page }));
        let msgs = mcp_app::card_with(&result, Some(&sign)).unwrap().messages;
        for m in &msgs {
            assert!(schema.validate(m).is_ok(), "{m}");
        }
        // The producer surface's id is random per answer: pinned as `-p`.
        let mut produced = pretty(&json!(msgs));
        for m in &msgs {
            if let Some(id) = m["createSurface"]["surfaceId"].as_str()
                && id != basic::SURFACE_ID
            {
                produced = produced.replace(id, "triton-a2ui-p");
            }
        }
        let path = golden(&format!("in-card-{page}"));
        if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
            std::fs::write(&path, &produced).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            produced,
            "{path:?} differs from golden"
        );
    }
}

/// The producer surface ids of a card, in the order the widget gets them.
fn producer_surfaces(res: &Value) -> Vec<String> {
    res["_meta"][mcp_app::META_KEY]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["createSurface"]["surfaceId"].as_str())
        .filter(|s| *s != basic::SURFACE_ID)
        .map(str::to_string)
        .collect()
}

/// Another endpoint advertising a tool's name with other annotations
/// changes nothing: the read-only gate reads the annotations of the
/// endpoint that executes the tool, and asks that endpoint only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_read_only_gate_reads_the_endpoint_that_runs_the_tool() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let decoy_probes = r.decoy.tools_list_probes();
    for _ in 0..3 {
        let w = call(&r, ALICE, Some(&session), "file_change", json!({})).await;
        assert!(
            card_events(&w).iter().all(|(n, _)| n != in_card::EVENT),
            "a writing tool got in-card controls: {w}"
        );
        let ro = call(&r, ALICE, Some(&session), "list_planned", json!({})).await;
        assert!(
            card_events(&ro).iter().any(|(n, _)| n == in_card::EVENT),
            "a read-only tool lost its in-card controls: {ro}"
        );
    }
    assert_eq!(
        r.decoy.tools_list_probes(),
        decoy_probes,
        "an endpoint that runs neither tool was asked about them"
    );
}

/// The check runs only for an answer that has a card to sign; a tool found
/// not read-only is logged once, not on every cache expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_read_only_check_is_lazy_and_warns_once() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let probes = r.echo.tools_list_probes();
    let out = call(&r, ALICE, Some(&session), "plain_reads", json!({})).await;
    assert_eq!(out["isError"], false, "{out}");
    assert_eq!(
        r.echo.tools_list_probes(),
        probes,
        "an answer without a card needs no read-only check"
    );

    call(&r, ALICE, Some(&session), "file_change", json!({})).await;
    // Past the "not read-only" cache lifetime: the list is read again, the
    // answer is the same, so there is nothing new to say.
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    call(&r, ALICE, Some(&session), "file_change", json!({})).await;
    let warnings = r
        .triton
        .stdout_snapshot()
        .iter()
        .filter(|l| l.contains("not annotated readOnlyHint") && l.contains("file_change"))
        .count();
    assert_eq!(warnings, 1, "one warning per change of state");
}

/// A producer token allows each event on the surface that carried it: an
/// event of one surface fired on another is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_producer_event_is_allowed_on_its_own_surface_only() {
    let r = rig().await;
    let session = card_session(&r, ALICE).await;
    let res = call(&r, ALICE, Some(&session), "two_panels", json!({})).await;
    let token = res["_meta"][in_card::META_KEY]
        .as_str()
        .unwrap_or_else(|| panic!("no producer token: {res}"))
        .to_string();
    let s = producer_surfaces(&res);
    assert_eq!(s.len(), 2, "{res}");
    let hits = r.panels.hits();
    for (name, surface) in [
        ("triton.in_card:next", &s[0]),
        ("triton.in_card:refresh", &s[1]),
    ] {
        let out = call(
            &r,
            ALICE,
            Some(&session),
            in_card::TOOL_NAME,
            ui_event(&token, name, surface, json!({})),
        )
        .await;
        refused(&out, "validation");
    }
    assert_eq!(r.panels.hits(), hits, "a refused event reaches no upstream");
    for (name, surface) in [
        ("triton.in_card:refresh", &s[0]),
        ("triton.in_card:next", &s[1]),
    ] {
        let out = call(
            &r,
            ALICE,
            Some(&session),
            in_card::TOOL_NAME,
            ui_event(&token, name, surface, json!({})),
        )
        .await;
        assert_eq!(out["isError"], false, "{name} on its own surface: {out}");
    }
}
