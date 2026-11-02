//! The canonical answer surface as an MCP Apps widget (ChatGPT, Claude).
//!
//! An MCP host that renders MCP Apps loads ONE static HTML resource for a
//! tool (`_meta.ui.resourceUri` on the tool descriptor) and hands it the
//! `tools/call` result. Triton maps the canonical surface to the widget's
//! data (`structuredContent.widget`, beside the unchanged envelope), so the
//! widget draws the same card anatomy as the Teams and Gemini mappings:
//! header, body, one primary and the secondary actions.
//!
//! * the mapping per fixture surface is pinned as `<name>.mcp.json`;
//! * the real binary on a customer path (`/mcp/<slug>`, Entra-style token)
//!   advertises the resource on `tools/list`, lists and serves it with the
//!   MCP Apps mime type, and carries the widget data on `tools/call`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use triton_core::a2ui::mcp_app;
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub)
}

fn read_json(p: &PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

fn fixture(name: &str) -> Value {
    read_json(&dir(&format!("fixtures/conversation/surfaces/{name}.json")))
}

fn fixture_names() -> Vec<String> {
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

fn golden(name: &str) -> PathBuf {
    dir(&format!("fixtures/conversation/golden/{name}.mcp.json"))
}

fn pretty(v: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap())
}

/// Every action the widget can fire, anywhere in its data.
fn actions(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            if let Some(a) = m.get("action").filter(|a| a.get("type").is_some()) {
                out.push(a.clone());
            }
            m.values().for_each(|x| actions(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| actions(x, out)),
        _ => {}
    }
}

#[test]
fn every_fixture_surface_maps_to_pinned_widget_data() {
    let mut failures = Vec::new();
    for name in fixture_names() {
        let data = mcp_app::widget_data(&fixture(&name))
            .unwrap_or_else(|| panic!("{name}: no widget data"));
        let produced = pretty(&data);
        let path = golden(&name);
        if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
            std::fs::write(&path, &produced).unwrap();
        } else {
            match std::fs::read_to_string(&path) {
                Ok(want) if want == produced => {}
                Ok(_) => failures.push(format!("{name}.mcp.json: differs from golden")),
                Err(_) => failures.push(format!("{name}.mcp.json: no golden file")),
            }
        }
        // One anatomy: the primary action leads the bar.
        let bar = data["actions"].as_array().cloned().unwrap_or_default();
        if let Some(k) = bar.iter().position(|a| a["primary"] == true) {
            assert_eq!(k, 0, "{name}: the primary action must lead");
        }
        // Every control is a stateless action: a complete message or an
        // https deep link. No tool re-invocation, no token.
        let mut all = Vec::new();
        actions(&data, &mut all);
        for a in all {
            match a["type"].as_str() {
                Some("send_message") => assert!(!a["text"].as_str().unwrap().trim().is_empty()),
                Some("open_url") => assert!(a["url"].as_str().unwrap().starts_with("https://")),
                other => panic!("{name}: unexpected action {other:?}"),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The five turn-3 shapes map to the blocks the widget draws.
#[test]
fn turn_three_shapes_map_to_their_blocks() {
    let kinds = |name: &str| -> Vec<String> {
        mcp_app::widget_data(&fixture(name)).unwrap()["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(kinds("3a"), ["table"]);
    assert_eq!(kinds("3b"), ["tree"]);
    assert_eq!(kinds("3c"), ["form"]);
    assert_eq!(kinds("3d"), ["form"]);
    assert_eq!(kinds("3e"), ["facts"]);
    assert_eq!(kinds("3g"), ["notice"]);

    let a = mcp_app::widget_data(&fixture("3a")).unwrap();
    assert_eq!(a["header"]["title"], "6 planned Agents");
    assert_eq!(a["header"]["stamp"], "as of 18:17");
    assert_eq!(a["actions"][0]["label"], "Open in the portfolio");
    assert_eq!(a["actions"][0]["primary"], true);
    assert_eq!(a["actions"].as_array().unwrap().len(), 3);

    // A change form: one group per thing to change, each with its template
    // as parts the widget fills exactly like the server would.
    let c = mcp_app::widget_data(&fixture("3c")).unwrap();
    let form = &c["body"][0];
    assert_eq!(form["submit_label"], "File for review");
    assert_eq!(form["groups"][0]["label"], "Owner");
    assert_eq!(
        form["groups"][0]["template"],
        triton_correlation::message::template_parts(
            "Set the owner of Agent 28 to \"{value}\".[[ Reason: {reason}]] (version 08cbcd825d41)"
        )
    );

    // A failure carries the attention tone on the whole card.
    let g = mcp_app::widget_data(&fixture("3g")).unwrap();
    assert_eq!(g["tone"], "attention");
}

#[test]
fn a_result_without_conversation_components_has_no_widget_data() {
    assert!(mcp_app::widget_data(&json!({ "text": "hello" })).is_none());
    assert!(
        mcp_app::widget_data(&json!({ "surface": { "components": [
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
        // MCP requires an object schema; strict clients (the MCP Apps
        // reference host's SDK) refuse the whole list otherwise.
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["_meta"]["ui"]["resourceUri"], uri, "{t}");
        assert_eq!(t["_meta"]["openai/outputTemplate"], uri, "{t}");
        // The flat `ui/resourceUri` is deprecated (removed before GA).
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

    // resources/read returns the self-contained HTML.
    let read = rpc(&r, "resources/read", json!({ "uri": uri })).await;
    let content = &read["contents"][0];
    assert_eq!(content["uri"], uri);
    assert_eq!(content["mimeType"], mcp_app::MIME_TYPE);
    csp(&content["_meta"]);
    let html = content["text"].as_str().expect("html text");
    assert!(
        html.starts_with("<!doctype html>"),
        "{}",
        &html[..80.min(html.len())]
    );
    assert_eq!(html, mcp_app::widget_html());
    // No external network: nothing loaded from anywhere.
    for needle in ["http://", "https://", "<link", "src="] {
        assert!(!html.contains(needle), "widget references {needle}");
    }

    // tools/call: the widget data matches the pinned mapping; the text and
    // the envelope are what every other host gets.
    for (tool, name) in SHAPES {
        let res = rpc(&r, "tools/call", json!({ "name": tool, "arguments": {} })).await;
        let want = read_json(&golden(name));
        assert_eq!(res["structuredContent"]["widget"], want, "{tool}");
        assert_eq!(
            res["content"][0]["text"],
            format!("full text of {name}"),
            "{tool}: text unchanged"
        );
        assert!(
            res["structuredContent"]["result"]["surface"].is_object(),
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
    assert!(res["structuredContent"].get("widget").is_none(), "{res}");
}

/// The widget composes a form's request on the client. These vectors pin
/// what every channel sends for a template and the user's inputs (the
/// server-side `resolve`, which Teams and Gemini use); the widget harness
/// runs the same file through the widget's own fill.
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
    // The browser harness runs the same vectors, with the parts the widget
    // would receive, through the widget's own fill.
    if let Ok(out) = std::env::var("WIDGET_FILL_VECTORS_OUT") {
        std::fs::write(out, serde_json::to_string(&with_parts).unwrap()).unwrap();
    }
}

/// A deep link the widget cannot open (anything but https) is no control.
#[test]
fn only_https_links_become_widget_controls() {
    let d = mcp_app::widget_data(&json!({ "surface": { "components": [
        { "kind": "header", "title": "T" },
        { "kind": "list", "items": [ { "label": "local",
          "action": { "type": "open_url", "url": "http://127.0.0.1:8080/x" } } ] }
    ] } }))
    .unwrap();
    assert!(d["body"][0]["items"][0].get("action").is_none(), "{d}");
    assert!(mcp_app::is_widget_action(
        &json!({ "type": "open_url", "url": "https://a.example/x" })
    ));
    assert!(!mcp_app::is_widget_action(
        &json!({ "type": "open_url", "url": "http://127.0.0.1/x" })
    ));
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
