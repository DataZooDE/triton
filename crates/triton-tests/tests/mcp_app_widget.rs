//! The canonical answer surface as an MCP Apps widget (ChatGPT, Claude), on
//! the official A2UI v0.9 renderer (#368).
//!
//! An MCP host that renders MCP Apps loads ONE static HTML resource for a
//! tool (`_meta.ui.resourceUri` on the tool descriptor) and hands it the
//! `tools/call` result. The page bundles `@a2ui/lit` (basic catalog); the
//! result's `_meta["triton/a2ui"]` (which the host hands the widget but not
//! the model) carries A2UI v0.9 messages: the producer's basic-catalog
//! passthrough surfaces, then Triton's card projected onto the basic
//! catalog (header, body, one primary and the secondary actions).
//!
//! * the projection per fixture surface is pinned as `<name>.a2ui.json`,
//!   and every message validates against the vendored basic catalog;
//! * the real binary on a customer path (`/mcp/<slug>`, Entra-style token)
//!   advertises the resource on `tools/list`, lists and serves it with the
//!   MCP Apps mime type, and carries the messages on `tools/call`, never in
//!   `content` or `structuredContent`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use triton_core::a2ui::{basic, mcp_app};

use crate::conversation_components::a2ui_basic_validator;
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

pub(crate) fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub)
}

pub(crate) fn read_json(p: &PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

pub(crate) fn fixture(name: &str) -> Value {
    read_json(&dir(&format!("fixtures/conversation/surfaces/{name}.json")))
}

pub(crate) fn fixture_names() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir("fixtures/conversation/surfaces"))
        .expect("fixture dir")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "json").then(|| p.file_stem().unwrap().to_string_lossy().to_string())
        })
        .collect();
    v.sort();
    v
}

pub(crate) fn golden(name: &str) -> PathBuf {
    dir(&format!("fixtures/conversation/golden/{name}.a2ui.json"))
}

pub(crate) fn pretty(v: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap())
}

// ── Reading the card back ──────────────────────────────────────────────

/// Triton's card: the messages of [`basic::SURFACE_ID`].
pub(crate) fn card_of(result: &Value) -> Option<Vec<Value>> {
    basic::project(result).map(|c| c.messages)
}

/// The components of Triton's card among `msgs`.
pub(crate) fn components(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .filter(|m| m["updateComponents"]["surfaceId"] == basic::SURFACE_ID)
        .flat_map(|m| {
            m["updateComponents"]["components"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

/// Every shown text of the card.
fn texts(msgs: &[Value]) -> Vec<String> {
    components(msgs)
        .iter()
        .filter(|c| c["component"] == "Text")
        .filter_map(|c| c["text"].as_str().map(String::from))
        .collect()
}

/// Every event a control of the card fires: `(name, context)`.
fn events(msgs: &[Value]) -> Vec<(String, Value)> {
    components(msgs)
        .iter()
        .filter_map(|c| c["action"].get("event"))
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["context"].clone(),
            )
        })
        .collect()
}

fn of_kind(msgs: &[Value], kind: &str) -> Vec<Value> {
    components(msgs)
        .into_iter()
        .filter(|c| c["component"] == kind)
        .collect()
}

/// Form submits: the events that carry template parts.
fn forms(msgs: &Option<Vec<Value>>) -> usize {
    msgs.as_ref().map_or(0, |m| {
        events(m)
            .iter()
            .filter(|(_, ctx)| ctx.get("parts").is_some())
            .count()
    })
}

/// The ids a component refers to (`child`, `children`, tab children).
fn refs(c: &Value) -> Vec<String> {
    let mut out: Vec<String> = c["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    out.extend(c["child"].as_str().map(String::from));
    out.extend(
        c["tabs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["child"].as_str().map(String::from)),
    );
    out
}

#[test]
fn every_fixture_surface_maps_to_pinned_basic_catalog_messages() {
    let schema = a2ui_basic_validator();
    let mut failures = Vec::new();
    for name in fixture_names() {
        let msgs = card_of(&fixture(&name)).unwrap_or_else(|| panic!("{name}: no card"));
        for m in &msgs {
            if let Err(e) = schema.validate(m) {
                failures.push(format!("{name}: invalid v0.9 basic message: {e}"));
            }
        }
        // A well-formed tree: unique ids, every reference resolves, one
        // `root`, every component reachable from it.
        let comps = components(&msgs);
        let ids: Vec<&str> = comps.iter().map(|c| c["id"].as_str().unwrap()).collect();
        let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "{name}: duplicate ids");
        assert!(unique.contains("root"), "{name}: no root");
        let mut seen = std::collections::BTreeSet::new();
        let mut todo = vec!["root".to_string()];
        while let Some(id) = todo.pop() {
            let c = comps
                .iter()
                .find(|c| c["id"] == id.as_str())
                .unwrap_or_else(|| panic!("{name}: dangling ref {id}"));
            if seen.insert(id) {
                todo.extend(refs(c));
            }
        }
        assert_eq!(seen.len(), ids.len(), "{name}: unreachable components");
        // Every control is a stateless event: a complete message, a form's
        // template, or an https deep link. No tool re-invocation, no token.
        for (ev, ctx) in events(&msgs) {
            match ev.as_str() {
                basic::SEND_MESSAGE => assert!(
                    ctx["text"].as_str().is_some_and(|t| !t.trim().is_empty())
                        || ctx["parts"].is_string(),
                    "{name}: {ctx}"
                ),
                basic::OPEN_URL => assert!(
                    ctx["url"].as_str().unwrap().starts_with("https://"),
                    "{name}: {ctx}"
                ),
                other => panic!("{name}: unexpected event {other}"),
            }
        }
        let produced = pretty(&json!(msgs));
        let path = golden(&name);
        if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
            std::fs::write(&path, &produced).unwrap();
        } else {
            match std::fs::read_to_string(&path) {
                Ok(want) if want == produced => {}
                Ok(_) => failures.push(format!("{name}.a2ui.json: differs from golden")),
                Err(_) => failures.push(format!("{name}.a2ui.json: no golden file")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The turn-3 shapes keep their anatomy on the basic catalog.
#[test]
fn turn_three_shapes_keep_their_anatomy() {
    // 3a: header, a table of 6 rows (and its header row), three actions,
    // the primary first.
    let a = card_of(&fixture("3a")).unwrap();
    let t = texts(&a);
    assert_eq!(t[0], "6 planned Agents", "{t:?}");
    assert!(t.iter().any(|x| x.contains("as of 18:17")), "{t:?}");
    let bar = components(&a)
        .into_iter()
        .rfind(|c| c["component"] == "Row")
        .unwrap();
    let buttons = refs(&bar);
    assert_eq!(buttons.len(), 3, "{bar}");
    let first = components(&a)
        .into_iter()
        .find(|c| c["id"] == buttons[0].as_str())
        .unwrap();
    assert_eq!(first["variant"], "primary", "{first}");

    // 3b: a tree, indented per level.
    let b = card_of(&fixture("3b")).unwrap();
    assert!(
        texts(&b).iter().any(|x| x.starts_with('\u{2003}')),
        "{:?}",
        texts(&b)
    );

    // 3c: a change form, one tab per group, each submit carrying its
    // template as the parts the widget fills like the server.
    let c = card_of(&fixture("3c")).unwrap();
    let tabs = of_kind(&c, "Tabs");
    assert_eq!(tabs.len(), 1, "{c:#?}");
    assert_eq!(tabs[0]["tabs"][0]["title"], "Owner");
    let (_, ctx) = events(&c)
        .into_iter()
        .find(|(_, ctx)| ctx.get("parts").is_some())
        .unwrap();
    let parts: Value = serde_json::from_str(ctx["parts"].as_str().unwrap()).unwrap();
    assert_eq!(
        parts,
        triton_correlation::message::template_parts(
            "Set the owner of Agent 28 to \"{value}\".[[ Reason: {reason}]] (version 08cbcd825d41)"
        )
    );
    assert!(ctx["v:value"]["path"].as_str().is_some(), "{ctx}");

    // 3g: a failure notice carries the warning mark.
    let g = card_of(&fixture("3g")).unwrap();
    assert!(
        texts(&g).iter().any(|x| x.starts_with('\u{26a0}')),
        "{:?}",
        texts(&g)
    );
}

#[test]
fn a_result_without_conversation_components_has_no_card() {
    assert!(card_of(&json!({ "text": "hello" })).is_none());
    assert!(
        card_of(&json!({ "surface": { "components": [
            { "kind": "text", "value": "just prose" }
        ] } }))
        .is_none()
    );
}

// ── The real binary ────────────────────────────────────────────────────

const ORIGIN: &str = "https://agents.example.test";
const ENTRA_AUD: &str = "api://triton-mcp";
const TID: &str = "11111111-2222-3333-4444-abcdefabcdef";

/// The goldens the binary test pins: list/table, map tree, change form,
/// receipt, failure — each served by its own upstream tool.
const SHAPES: [(&str, &str); 5] = [
    ("list_agents", "3a"),
    ("capability_map", "3b"),
    ("change_form", "3c"),
    ("receipt", "3e"),
    ("failure", "3g"),
];

pub(crate) struct Rig {
    pub(crate) triton: TritonProcess,
    pub(crate) entra: TestIssuer,
    _agents: Vec<FakeAgent>,
}

pub(crate) async fn rig() -> Rig {
    let entra = TestIssuer::start().await;
    let mut agents = Vec::new();
    let mut upstreams = Vec::new();
    for (tool, name) in SHAPES {
        let mut body = fixture(name);
        body.as_object_mut().unwrap().remove("_mock_item");
        body["text"] = json!(format!("full text of {name}"));
        let agent = FakeAgent::start_returning(body).await;
        upstreams.push(format!("{tool}={}", agent.host_port()));
        agents.push(agent);
    }
    // A prose-only answer: no card, so no widget data.
    let prose = FakeAgent::start_returning(json!({ "text": "just prose" })).await;
    upstreams.push(format!("prose={}", prose.host_port()));
    agents.push(prose);
    // A card with rows beside prose the card does not show: a narration, a
    // chart and sources (see `mcp_card_text`).
    let narrated = FakeAgent::start_returning(json!({ "surface": { "components": [
        { "kind": "header", "title": "3 Things" },
        { "kind": "text", "value": "3 Things:\n- A\n- B\n- C" },
        { "kind": "narration", "text": "Results exclude the archived system." },
        { "kind": "list", "items": [ { "label": "A" }, { "label": "B" }, { "label": "C" } ] },
        { "kind": "report", "image_url": "https://img.example/c.png" },
        { "kind": "sources", "items": [ { "label": "Doc", "resource": "https://docs.example/d" } ] }
    ] } }))
    .await;
    upstreams.push(format!("narrated={}", narrated.host_port()));
    agents.push(narrated);
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
        ("TRITON_STATIC_UPSTREAMS".to_string(), upstreams.join(",")),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    Rig {
        triton,
        entra,
        _agents: agents,
    }
}

pub(crate) fn token(entra: &TestIssuer) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD,
        "sub": "entra-pairwise-sub",
        "oid": "00000000-0000-0000-0000-0000000000aa",
        "tid": TID,
        "preferred_username": "bob@acme.example",
        "exp": now + 600,
        "iat": now,
        "scp": "access_as_user",
    }))
}

async fn rpc(r: &Rig, method: &str, params: Value) -> Value {
    let resp = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(token(&r.entra))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await
        .expect("POST /mcp/acme");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "{method}");
    let v: Value = resp.json().await.expect("json");
    assert!(v.get("error").is_none(), "{method}: {v}");
    v["result"].clone()
}

/// A page fit for an MCP Apps host frame: self-contained, a strict CSP of
/// its own, and within the size budget.
pub(crate) const MAX_WIDGET_BYTES: usize = 768 * 1024;

pub(crate) fn assert_self_contained(html: &str) {
    assert!(
        html.to_ascii_lowercase().starts_with("<!doctype html>"),
        "{}",
        &html[..80.min(html.len())]
    );
    assert!(
        html.len() <= MAX_WIDGET_BYTES,
        "widget is {} bytes",
        html.len()
    );
    assert!(html.contains("connect-src 'none'"), "no CSP meta");
    let lower = html.to_ascii_lowercase();
    for needle in [
        "<link",
        "@import",
        "src=\"http",
        "src='http",
        "src=http",
        "href=\"http",
        "url(http",
        "url(\"http",
        "url('http",
    ] {
        assert!(
            !lower.contains(needle),
            "widget loads from outside: {needle}"
        );
    }
    assert!(html.contains("triton-a2ui-widget"), "the build marker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_customer_mcp_path_serves_the_answer_card_widget() {
    let r = rig().await;

    // tools/list: every surface-answering tool names the widget, in the
    // MCP Apps key and the ChatGPT Apps SDK key.
    let list = rpc(&r, "tools/list", json!({})).await;
    let tools: Vec<&Value> = list["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|t| SHAPES.iter().any(|(n, _)| t["name"] == *n))
        .collect();
    assert_eq!(tools.len(), SHAPES.len(), "{list}");
    let uri = tools[0]["_meta"]["ui"]["resourceUri"]
        .as_str()
        .unwrap_or_else(|| panic!("no _meta.ui.resourceUri: {list}"))
        .to_string();
    assert!(uri.starts_with("ui://"), "{uri}");
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["_meta"]["ui"]["resourceUri"], uri, "{t}");
        assert_eq!(t["_meta"]["openai/outputTemplate"], uri, "{t}");
        assert!(t["_meta"].get("ui/resourceUri").is_none(), "{t}");
    }

    // resources/list names it with the MCP Apps mime type.
    let resources = rpc(&r, "resources/list", json!({})).await;
    let entry = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["uri"] == uri)
        .unwrap_or_else(|| panic!("{uri} not listed: {resources}"))
        .clone();
    assert_eq!(entry["mimeType"], mcp_app::MIME_TYPE);
    // Both CSP spellings: the MCP Apps one and ChatGPT's own, empty (the
    // page loads nothing), on the list entry and on the read content.
    let csp = |m: &Value| {
        assert_eq!(
            m["ui"]["csp"],
            json!({ "connectDomains": [], "resourceDomains": [] }),
            "{m}"
        );
        assert_eq!(
            m["openai/widgetCSP"],
            json!({ "connect_domains": [], "resource_domains": [] }),
            "{m}"
        );
    };
    csp(&entry["_meta"]);

    // resources/read returns the self-contained page with the renderer.
    let read = rpc(&r, "resources/read", json!({ "uri": uri })).await;
    let content = &read["contents"][0];
    assert_eq!(content["uri"], uri);
    assert_eq!(content["mimeType"], mcp_app::MIME_TYPE);
    csp(&content["_meta"]);
    let html = content["text"].as_str().expect("html text");
    assert_eq!(html, mcp_app::widget_html());
    assert_self_contained(html);

    // tools/call (a plain client): the card's v0.9 messages ride the result
    // `_meta`, which the host hands the widget but not the model; nothing
    // A2UI reaches `content` or `structuredContent`.
    for (tool, name) in SHAPES {
        let res = rpc(&r, "tools/call", json!({ "name": tool, "arguments": {} })).await;
        let want = read_json(&golden(name));
        assert_eq!(res["_meta"][mcp_app::META_KEY], want, "{tool}");
        assert!(res["_meta"].get("triton/widget").is_none(), "{tool}");
        assert_no_a2ui_for_the_model(&res);
        assert_eq!(
            res["content"][0]["text"],
            format!("full text of {name}"),
            "{tool}: text unchanged"
        );
        assert!(
            res["structuredContent"]["result"]["surface"]["components"].is_array(),
            "{tool}: envelope unchanged"
        );
    }
    let res = rpc(
        &r,
        "tools/call",
        json!({ "name": "prose", "arguments": {} }),
    )
    .await;
    assert_eq!(res["content"][0]["text"], "just prose");
    assert!(res["_meta"].get(mcp_app::META_KEY).is_none(), "{res}");
}

/// What the model reads (`content`, `structuredContent`) never carries A2UI
/// messages or the producer's passthrough surfaces.
pub(crate) fn assert_no_a2ui_for_the_model(res: &Value) {
    let model = json!([res["content"], res["structuredContent"]]).to_string();
    for needle in ["createSurface", "updateComponents", "\"a2ui\""] {
        assert!(
            !model.contains(needle),
            "{needle} reaches the model: {model}"
        );
    }
}

/// A Claude-like host (2026-07-28, declares MCP Apps per request) asks a
/// tool whose agent answers with prose, a basic-catalog surface of its own,
/// a Gemini-only surface and a canonical card: the widget gets the basic
/// surface verbatim, then Triton's card, all as v0.9 basic-catalog
/// messages; the Gemini-only surface stays out; the model gets the card
/// text and no A2UI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_host_gets_producer_surfaces_and_the_card_as_v09_messages() {
    let basic_surface = json!({
        "catalogId": triton_core::a2ui::passthrough::BASIC_CATALOG_ID,
        "components": [
            { "id": "root", "component": "Column", "children": ["t", "b"] },
            { "id": "t", "component": "Text", "text": "Pick a class." },
            { "id": "bl", "component": "Text", "text": "Apply" },
            { "id": "b", "component": "Button", "child": "bl",
              "action": { "event": { "name": "apply", "context": { "pick": { "path": "/pick" } } } } }
        ],
        "data_model": { "pick": "a" }
    });
    let agent = FakeAgent::start_returning(json!({ "surface": {
        "components": [
            { "kind": "text", "value": "Three close calls." },
            { "kind": "header", "title": "Close calls" },
            { "kind": "list", "items": [ { "label": "Claim 1" }, { "label": "Claim 2" } ] }
        ],
        "a2ui": [
            basic_surface.clone(),
            { "components": [ { "id": "root", "component": "MaterialCard", "children": [] } ] },
            // A producer surface firing Triton's own event: dropped.
            { "catalogId": triton_core::a2ui::passthrough::BASIC_CATALOG_ID, "components": [
                { "id": "root", "component": "Button", "child": "l",
                  "action": { "event": { "name": "triton.send_message",
                                         "context": { "text": "spoofed" } } } },
                { "id": "l", "component": "Text", "text": "Spoof" } ] }
        ]
    }}))
    .await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("close_calls={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let claude_like = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "claude-ai", "version": "1.0.0" },
        "io.modelcontextprotocol/clientCapabilities": { "extensions": {
            "io.modelcontextprotocol/ui": { "mimeTypes": [mcp_app::MIME_TYPE] } } }
    });
    let resp: Value = reqwest::Client::new()
        .post(triton.rest_url("/mcp"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth("dev-token")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "close_calls", "arguments": { "message": "Which claims are close?" },
            "_meta": claude_like } }),
        )
        .send()
        .await
        .expect("POST /mcp")
        .json()
        .await
        .expect("json");
    let res = &resp["result"];
    let msgs = res["_meta"][mcp_app::META_KEY]
        .as_array()
        .unwrap_or_else(|| panic!("no {}: {resp}", mcp_app::META_KEY));
    let schema = a2ui_basic_validator();
    for m in msgs {
        schema.validate(m).unwrap_or_else(|e| panic!("{e}: {m}"));
    }
    let creates: Vec<&Value> = msgs.iter().filter_map(|m| m.get("createSurface")).collect();
    assert_eq!(
        creates.len(),
        2,
        "the basic surface and the card: {msgs:#?}"
    );
    assert_eq!(creates[1]["surfaceId"], basic::SURFACE_ID);
    let producer = msgs
        .iter()
        .find(|m| m["updateComponents"]["surfaceId"] == creates[0]["surfaceId"])
        .unwrap();
    assert_eq!(
        producer["updateComponents"]["components"], basic_surface["components"],
        "verbatim"
    );
    assert!(
        msgs.iter()
            .any(|m| m["updateDataModel"]["value"]["pick"] == "a"),
        "{msgs:#?}"
    );
    assert!(
        !json!(msgs).to_string().contains("MaterialCard"),
        "GE-only stays out"
    );
    assert!(
        !json!(msgs).to_string().contains("spoofed"),
        "reserved events dropped"
    );
    assert!(res["_meta"].get("triton/widget").is_none());
    assert_no_a2ui_for_the_model(res);
    assert_eq!(
        res["content"][0]["text"],
        format!(
            "Close calls: Claim 1, Claim 2 \u{2014} shown in the card.\n{}",
            mcp_app::CARD_NOTE
        ),
        "{res}"
    );
}

/// The widget composes a form's request on the client. These vectors pin
/// what every channel sends for a template and the user's inputs (the
/// server-side `resolve`, which Teams and Gemini use); the widget's own
/// fill runs the same vectors (`fill_vectors.parts.json`).
#[test]
fn fill_vectors_match_the_server_side_resolve() {
    use triton_correlation::message::{MessageError, resolve, template_args};
    let vectors = read_json(&dir("fixtures/conversation/fill_vectors.json"));
    let mut with_parts = Vec::new();
    for v in vectors.as_array().unwrap() {
        let tpl = v["template"].as_str().unwrap();
        let values = v["values"].as_object().unwrap();
        let got = resolve(&template_args(tpl), |k| {
            values.get(k).and_then(Value::as_str).map(str::to_string)
        });
        match (v.get("text"), v.get("missing")) {
            (Some(t), _) => assert_eq!(got.as_deref().ok(), t.as_str(), "{tpl}"),
            (_, Some(m)) => {
                let want: Vec<String> = serde_json::from_value(m.clone()).unwrap();
                assert_eq!(got, Err(MessageError::Missing(want)), "{tpl}");
            }
            _ => panic!("vector needs text or missing"),
        }
        let mut v = v.clone();
        v["parts"] = triton_correlation::message::template_parts(tpl);
        with_parts.push(v);
    }
    // The widget's own fill (packages/mcp-widget, `npm test`) runs the same
    // vectors with the parts the widget receives: pinned here, so the two
    // sides cannot drift apart.
    let produced = pretty(&json!(with_parts));
    let path = dir("fixtures/conversation/fill_vectors.parts.json");
    if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
        std::fs::write(&path, &produced).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("fill_vectors.parts.json"),
        produced,
        "fill_vectors.parts.json is stale: rerun with UPDATE_CONVERSATION_GOLDEN=1"
    );
}

/// A deep link the widget cannot open (anything but https) is no control.
#[test]
fn only_https_links_become_widget_controls() {
    let d = card_of(&json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "list", "items": [ { "label": "local",
          "action": { "type": "open_url", "url": "http://127.0.0.1:8080/x" } },
          { "label": "good", "action": { "type": "open_url", "url": "https://a.example/x" } } ] }
    ] } }))
    .unwrap();
    let ev = events(&d);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].1["url"], "https://a.example/x");
}

#[test]
fn an_unsafe_link_never_becomes_a_control() {
    let d = card_of(&json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "button", "label": "x",
          "action": { "type": "open_url", "url": "javascript:alert(1)" } },
        { "kind": "list", "items": [ { "label": "r",
          "action": { "type": "open_url", "url": "file:///etc/passwd" } } ] }
    ] } }))
    .unwrap();
    assert!(events(&d).is_empty(), "{d:#?}");
}

/// A chip whose message is blank would send an empty turn: no chip.
#[test]
fn a_blank_chip_is_no_control() {
    let d = card_of(&json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "chips", "chips": [
            { "label": "blank", "message": "   " },
            { "label": "ok", "message": "Show it" } ] },
        { "kind": "chips", "chips": [ { "label": "only blank", "message": "" } ] },
        { "kind": "tree", "nodes": [ { "label": "n", "chips": [
            { "label": "blank", "message": " " } ] } ] }
    ] } }))
    .unwrap();
    let ev = events(&d);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].1["text"], "Show it");
    assert!(!texts(&d).iter().any(|t| t == "only blank" || t == "blank"));
}

/// The card has one header: the surface's first, not one an embed's
/// fallback (or a second header) brings later.
#[test]
fn the_first_header_wins() {
    let d = card_of(&json!({ "surface": { "components": [
        { "kind": "header", "title": "First" },
        { "kind": "embed", "url": "https://a.example/x", "title": "Open",
          "fallback": { "kind": "header", "title": "Fallback" } }
    ] } }))
    .unwrap();
    assert_eq!(texts(&d)[0], "First");
    assert!(!texts(&d).iter().any(|t| t == "Fallback"));
}

/// Hosts fetch the widget out of band (ChatGPT at "Refresh tools", Claude
/// when it first renders), and none of these reads is a dispatch, so none
/// is audited. Each one is logged — method, uri, client — so a widget that
/// does not open can be traced to whether the host ever asked for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resource_reads_and_lists_are_logged_with_the_client() {
    let r = rig().await;
    let uri = mcp_app::resource_uri();
    let resp = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .header("user-agent", "openai-mcp/1.0.0")
        .bearer_auth(token(&r.entra))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": { "uri": uri } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    rpc(&r, "resources/list", json!({})).await;
    rpc(&r, "tools/list", json!({})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lines = r.triton.stdout_snapshot();
    let find = |method: &str| {
        lines
            .iter()
            .find(|l| {
                l.contains("\"mcp request\"") && l.contains(&format!("\"method\":\"{method}\""))
            })
            .cloned()
            .unwrap_or_else(|| panic!("no log line for {method}:\n{}", lines.join("\n")))
    };
    let read = find("resources/read");
    assert!(read.contains(uri), "{read}");
    assert!(read.contains("openai-mcp/1.0.0"), "{read}");
    assert!(read.contains("\"slug\":\"acme\""), "{read}");
    find("resources/list");
    find("tools/list");

    // A host fetching the widget without a usable bearer is refused, and
    // that is logged with the client too, or a failed template fetch would
    // leave no trace beyond an anonymous `rpc` audit line.
    let resp = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("user-agent", "openai-mcp/1.0.0 (widget)")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": { "uri": uri } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lines = r.triton.stdout_snapshot();
    let refused = lines
        .iter()
        .find(|l| l.contains("\"mcp request refused\"") && l.contains("openai-mcp/1.0.0 (widget)"))
        .unwrap_or_else(|| panic!("no refusal line:\n{}", lines.join("\n")));
    assert!(
        refused.contains("\"method\":\"resources/read\""),
        "{refused}"
    );
    assert!(refused.contains(uri), "{refused}");
}

// ── Budgets ────────────────────────────────────────────────────────────

/// A chain `levels` deep under each of `width` roots, `fan` children per
/// level: `width * (fan^levels - 1) / (fan - 1)` nodes.
fn tree(levels: usize, fan: usize, prefix: &str) -> Vec<Value> {
    if levels == 0 {
        return vec![];
    }
    (0..fan)
        .map(|i| {
            let label = format!("{prefix}.{i}");
            json!({ "label": label, "children": tree(levels - 1, fan, &label),
                    "action": { "type": "send_message", "text": format!("Show {label}") } })
        })
        .collect()
}

/// An oversized answer: more rows, deeper and wider trees, longer texts
/// than the widget budgets allow.
fn oversized() -> Value {
    let long = "x".repeat(2000);
    let rows: Vec<Value> = (0..120)
        .map(|i| json!({ "cells": { "a": format!("Row {i}"), "b": if i == 0 { long.clone() } else { "b".into() } },
                         "action": { "type": "send_message", "text": format!("Show row {i}") } }))
        .collect();
    let items: Vec<Value> = (0..80)
        .map(|i| json!({ "label": format!("Item {i}") }))
        .collect();
    let pairs: Vec<Value> = (0..70)
        .map(|i| json!({ "label": format!("Key {i}"), "value": format!("Value {i}") }))
        .collect();
    json!({ "surface": { "components": [
        { "kind": "header", "title": long.clone() },
        { "kind": "table", "columns": [ { "key": "a", "label": "A" }, { "key": "b", "label": "B" } ], "rows": rows },
        { "kind": "list", "items": items },
        { "kind": "key_value", "pairs": pairs },
        // 2 * (2^7 - 1) = 254 nodes, 7 levels deep.
        { "kind": "tree", "nodes": tree(7, 2, "n") },
        { "kind": "chips", "chips": [
            { "label": "fits", "message": "Show it" },
            { "label": "too long to send", "message": "m".repeat(501) } ] },
        { "kind": "button", "label": "Open", "primary": true,
          "action": { "type": "open_url", "url": "https://app.example.com/acme/open" } },
        { "kind": "button", "label": "Too long a link",
          "action": { "type": "open_url", "url": format!("https://app.example.com/{}", "p".repeat(3000)) } }
    ] } })
}

#[test]
fn an_oversized_answer_is_cut_to_the_widget_budgets_visibly() {
    let card = basic::project(&oversized()).expect("a card");
    let d = &card.messages;
    let t = texts(d);
    // Rows, items, pairs: at most 50 each, the rest counted as "more".
    for more in [70, 30, 20] {
        assert!(
            t.contains(&format!("\u{2026} {more} more")),
            "{more}: {t:?}"
        );
    }
    let row_events = events(d)
        .iter()
        .filter(|(_, c)| {
            c["text"]
                .as_str()
                .is_some_and(|x| x.starts_with("Show row"))
        })
        .count();
    assert_eq!(row_events, basic::MAX_ROWS);
    assert_eq!(
        t.iter().filter(|x| x.starts_with("Item ")).count(),
        basic::MAX_ROWS
    );

    // A tree: at most 4 levels and 200 nodes; the rest counted.
    let nodes: Vec<&String> = t.iter().filter(|x| x.contains("n.")).collect();
    assert!(nodes.len() <= basic::MAX_TREE_NODES, "{}", nodes.len());
    let deepest = nodes
        .iter()
        .map(|x| x.chars().take_while(|c| *c == '\u{2003}').count() + 1)
        .max()
        .unwrap();
    assert!(deepest <= basic::MAX_TREE_DEPTH, "{deepest}");
    let tree_more = 254 - nodes.len();
    assert!(t.contains(&format!("\u{2026} {tree_more} more")), "{t:?}");

    // Any text at most 500 characters, visibly cut.
    assert!(t.iter().all(|x| x.chars().count() <= basic::MAX_TEXT));
    assert!(t[0].ends_with('\u{2026}'), "{}", t[0]);

    // A message or link too long to show whole is no control.
    assert!(
        !t.iter()
            .any(|x| x == "too long to send" || x == "Too long a link")
    );

    // The whole card fits the byte budget, and is marked cut.
    assert!(serde_json::to_vec(d).unwrap().len() <= basic::MAX_BYTES);
    assert!(card.summary.cut);

    if let Ok(out) = std::env::var("WIDGET_OVERSIZED_OUT") {
        std::fs::write(out, serde_json::to_string(d).unwrap()).unwrap();
    }
}

/// Past the byte budget the card drops whole trailing blocks and says how
/// many: a reader sees that the card is incomplete.
#[test]
fn a_card_over_the_byte_budget_drops_trailing_blocks_visibly() {
    let mut components = vec![json!({ "kind": "header", "title": "Big" })];
    for b in 0..20 {
        let rows: Vec<Value> = (0..50)
            .map(|i| json!({ "cells": { "a": format!("{b}/{i} {}", "y".repeat(400)) } }))
            .collect();
        components.push(
            json!({ "kind": "table", "columns": [ { "key": "a", "label": "A" } ], "rows": rows }),
        );
    }
    let d = card_of(&json!({ "surface": { "components": components } })).unwrap();
    assert!(serde_json::to_vec(&d).unwrap().len() <= basic::MAX_BYTES);
    let kept = (0..20)
        .filter(|b| texts(&d).iter().any(|t| t.starts_with(&format!("{b}/0 "))))
        .count();
    assert!((1..20).contains(&kept), "{kept}");
    assert!(
        texts(&d).contains(&format!("\u{2026} {} more", 20 - kept)),
        "{:?}",
        texts(&d).last()
    );
}

// ── Forms ──────────────────────────────────────────────────────────────

fn form_with(fields: Value, template: &str) -> Value {
    json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "form", "title": "F", "submit_label": "Go", "fields": fields,
          "send_template": template }
    ] } })
}

/// A form's inputs are bound to the data model, seeded with the defaults,
/// and the submit binds every input into its event's context.
#[test]
fn a_form_binds_its_inputs_and_seeds_the_defaults() {
    let d = card_of(&form_with(
        json!([
            { "name": "who", "label": "Owner", "kind": "select", "required": true, "default": "ops",
              "options": [ { "label": "Ops", "value": "ops" }, { "label": "IT", "value": "it" } ] },
            { "name": "why", "label": "Reason", "kind": "text" },
            { "name": "urgent", "label": "Urgent", "kind": "boolean" }
        ]),
        "Set owner {who}.[[ Reason: {why}]] Urgent: {urgent}",
    ))
    .unwrap();
    let model = d
        .iter()
        .find_map(|m| m["updateDataModel"]["value"].as_object().cloned())
        .expect("data model");
    let form = model.values().next().unwrap();
    // Bound to generated keys, in field order; the submit names them.
    assert_eq!(form["i0"], json!(["ops"]));
    assert_eq!(form["i1"], "");
    assert_eq!(form["i2"], json!([]), "a boolean is unanswered, not false");
    let (_, ctx) = events(&d).pop().unwrap();
    for (f, key) in [("who", "i0"), ("why", "i1"), ("urgent", "i2")] {
        let path = ctx[format!("v:{f}")]["path"].as_str().unwrap();
        assert!(path.ends_with(&format!("/{key}")), "{path}");
    }
    assert_eq!(
        of_kind(&d, "ChoicePicker").len(),
        2,
        "the select and the boolean"
    );
    assert!(of_kind(&d, "CheckBox").is_empty());
    assert_eq!(of_kind(&d, "TextField")[0]["variant"], "longText");
}

/// A default is sent when the person does not touch the field: like any
/// payload it is never cut, so a group with an over-long default is none.
#[test]
fn an_over_long_default_is_no_form_rather_than_a_cut_one() {
    let d = card_of(&form_with(
        json!([ { "name": "r", "label": "Reason", "kind": "text", "default": "d".repeat(600) } ]),
        "Note {r}",
    ));
    assert_eq!(forms(&d), 0, "{d:?}");
    let ok = card_of(&form_with(
        json!([ { "name": "r", "label": "Reason", "kind": "text", "default": "d".repeat(500) } ]),
        "Note {r}",
    ));
    assert_eq!(forms(&ok), 1, "{ok:?}");
}

/// A form that can never be sent is no form: a template field without an
/// input, two inputs with one name, a required select without options.
#[test]
fn a_form_that_cannot_be_sent_is_no_form() {
    let missing = form_with(
        json!([ { "name": "a", "label": "A", "kind": "string" } ]),
        "Set {a} {b}",
    );
    assert_eq!(forms(&card_of(&missing)), 0);
    let twice = form_with(
        json!([ { "name": "a", "label": "A", "kind": "string" },
                { "name": "a", "label": "A2", "kind": "string" } ]),
        "Set {a}",
    );
    assert_eq!(forms(&card_of(&twice)), 0);
    let empty = form_with(
        json!([ { "name": "a", "label": "A", "kind": "select", "required": true,
                  "options": [ { "label": "long", "value": "v".repeat(501) } ] } ]),
        "Set {a}",
    );
    assert_eq!(forms(&card_of(&empty)), 0);
}

/// Bidi controls can make a sent text read differently from what it is:
/// a message or template that carries one is no control.
#[test]
fn a_message_with_bidi_controls_is_no_control() {
    let d = card_of(&json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "chips", "chips": [
            { "label": "ok", "message": "Show it" },
            { "label": "evil", "message": "Show \u{202e}ti eteled" } ] }
    ] } }))
    .unwrap();
    assert_eq!(events(&d).len(), 1, "{d:#?}");
    let f = form_with(
        json!([ { "name": "a", "label": "A", "kind": "string" } ]),
        "Set \u{2066}{a}",
    );
    assert_eq!(forms(&card_of(&f)), 0);
}

/// The widget's link rule is the parsed one: https with a host and no
/// credentials. An embed whose page fails it keeps only its fallback.
#[test]
fn a_link_with_credentials_or_no_host_is_no_control_anywhere() {
    for url in [
        "https://good.example@evil.example/x",
        "https://",
        "https:///x",
    ] {
        let d = card_of(&json!({ "surface": { "components": [
            { "kind": "embed", "url": url, "title": "Open page",
              "fallback": { "kind": "header", "title": "Fallback" } }
        ] } }))
        .unwrap();
        assert_eq!(texts(&d)[0], "Fallback", "{url}");
        assert!(events(&d).is_empty(), "{url}: {d:#?}");
    }
}

/// Many small blocks: the byte budget holds and the cut is counted.
#[test]
fn many_blocks_fit_the_byte_budget_quickly() {
    let mut components = vec![json!({ "kind": "header", "title": "Many" })];
    for i in 0..20_000 {
        components.push(json!({ "kind": "notice", "tone": "info", "text": format!("n{i}") }));
    }
    let t = std::time::Instant::now();
    let d = card_of(&json!({ "surface": { "components": components } })).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(serde_json::to_vec(&d).unwrap().len() <= basic::MAX_BYTES);
    let kept = texts(&d)
        .iter()
        .filter(|x| x.len() > 1 && x.starts_with('n') && x[1..].chars().all(|c| c.is_ascii_digit()))
        .count();
    let more = texts(&d)
        .iter()
        .find_map(|x| {
            x.strip_prefix("\u{2026} ")?
                .strip_suffix(" more")?
                .parse::<usize>()
                .ok()
        })
        .expect("a more line");
    assert_eq!(kept + more, 20_000);
}

/// A producer's field name is never a data-model path segment: names the
/// renderer refuses (`constructor`) or reads as a pointer (`a/b`) still
/// work, bound to generated keys; the submit maps them back.
#[test]
fn form_inputs_bind_to_generated_keys() {
    let d = card_of(&form_with(
        json!([ { "name": "constructor", "label": "C", "kind": "string" },
                { "name": "a/b", "label": "AB", "kind": "string" } ]),
        "Set {constructor} and {a/b}",
    ))
    .unwrap();
    let (_, ctx) = events(&d).pop().unwrap();
    for f in ["constructor", "a/b"] {
        let path = ctx[format!("v:{f}")]["path"].as_str().unwrap();
        assert!(
            !path.contains("constructor") && path.matches('/').count() == 2,
            "{f}: {path}"
        );
    }
    let model = d
        .iter()
        .find_map(|m| m["updateDataModel"]["value"].as_object().cloned())
        .unwrap();
    let keys: Vec<&String> = model
        .values()
        .next()
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert!(keys.iter().all(|k| k.starts_with('i')), "{keys:?}");
}

/// A boolean is three-state, as on every other channel: unanswered until
/// chosen (so a required one blocks the submit), never a silent `false`.
#[test]
fn a_boolean_is_unanswered_until_chosen() {
    let d = card_of(&form_with(
        json!([ { "name": "u", "label": "Urgent", "kind": "boolean", "required": true },
                { "name": "d", "label": "Done", "kind": "boolean", "default": true } ]),
        "Urgent {u}, done {d}",
    ))
    .unwrap();
    assert!(of_kind(&d, "CheckBox").is_empty(), "no two-state box");
    let pickers = of_kind(&d, "ChoicePicker");
    assert_eq!(pickers.len(), 2);
    let values: Vec<&Value> = pickers[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["value"])
        .collect();
    assert_eq!(values, [&json!("true"), &json!("false")]);
    let model = d
        .iter()
        .find_map(|m| m["updateDataModel"]["value"].as_object().cloned())
        .unwrap();
    let form = model.values().next().unwrap();
    let seeded: Vec<&Value> = form.as_object().unwrap().values().collect();
    assert_eq!(seeded, [&json!([]), &json!(["true"])], "{form}");
}

/// The producer's surfaces have a byte budget too: past it, trailing
/// surfaces go (Triton's own card stays).
#[test]
fn producer_surfaces_fit_a_byte_budget() {
    let big = |i: usize| {
        json!({ "catalogId": triton_core::a2ui::passthrough::BASIC_CATALOG_ID,
                "components": [ { "id": "root", "component": "Text", "text": format!("{i} {}", "z".repeat(20_000)) } ] })
    };
    let c = mcp_app::card(&json!({ "surface": {
        "components": [ { "kind": "header", "title": "Own" } ],
        "a2ui": (0..8).map(big).collect::<Vec<_>>()
    }}))
    .unwrap();
    let producer: Vec<&Value> = c
        .messages
        .iter()
        .filter(|m| {
            m["createSurface"]["surfaceId"]
                .as_str()
                .is_some_and(|s| s != basic::SURFACE_ID)
        })
        .collect();
    assert!((1..8).contains(&producer.len()), "{}", producer.len());
    let size = serde_json::to_vec(&c.messages).unwrap().len();
    assert!(size <= 2 * basic::MAX_BYTES, "{size}");
    assert!(
        texts(&c.messages).contains(&"Own".to_string()),
        "the card stays"
    );
}
