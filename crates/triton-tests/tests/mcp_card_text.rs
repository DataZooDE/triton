//! The model-visible text on a host that renders the answer card.
//!
//! A host that renders MCP Apps shows the card's rows to the user. If the
//! tool result's text carries the same rows, the host's model repeats them
//! as its own table under the card. So for a client that declared the MCP
//! Apps extension (`io.modelcontextprotocol/ui` with the
//! `text/html;profile=mcp-app` mime type), the text is one line — the
//! card's title and its row names — and a note to the model. Every other
//! client keeps the full text.
//!
//! * the card text per fixture surface is pinned in `card_texts.json` (built
//!   from the basic-catalog card's summary, #368: the same text as before);
//! * the real binary: a handshake-era client declares the extension in
//!   `initialize` and gets a session id that carries it; a 2026-07-28
//!   client declares it per request (`_meta` client capabilities).

use serde_json::{Value, json};
use triton_core::a2ui::mcp_app;

use crate::mcp_app_widget::{
    Rig, dir, fixture, fixture_names, golden, pretty, read_json, rig, token,
};

/// What a card-rendering host's model reads instead of the full text, per
/// fixture surface: the card's title and its row names, or `null` where
/// the card has no rows (the full text stays).
#[test]
fn every_fixture_surface_has_a_pinned_card_text() {
    let mut produced = serde_json::Map::new();
    for name in fixture_names() {
        let summary = mcp_app::card(&fixture(&name)).unwrap().summary.unwrap();
        let text = mcp_app::card_text(&summary, mcp_app::CARD_NOTE);
        if let Some(t) = &text {
            assert!(t.ends_with(mcp_app::CARD_NOTE), "{name}: {t}");
            assert_eq!(t.lines().count(), 2, "{name}: one line and the note: {t}");
        }
        produced.insert(name, json!(text));
    }
    let produced = pretty(&Value::Object(produced));
    let path = dir("fixtures/conversation/golden/card_texts.json");
    if std::env::var("UPDATE_CONVERSATION_GOLDEN").is_ok() {
        std::fs::write(&path, &produced).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("card_texts.json"),
        produced,
        "card texts differ from golden"
    );
}

const UI: &str = "io.modelcontextprotocol/ui";

fn ui_capabilities() -> Value {
    json!({ "extensions": { UI: { "mimeTypes": [mcp_app::MIME_TYPE] } } })
}

struct Reply {
    session: Option<String>,
    body: Value,
}

async fn post(r: &Rig, session: Option<&str>, method: &str, params: Value) -> Reply {
    let mut req = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(token(&r.entra))
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
    Reply { session, body }
}

async fn initialize(r: &Rig, capabilities: Value) -> Option<String> {
    post(
        r,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": capabilities,
            "clientInfo": { "name": "card-text-test", "version": "1.0.0" }
        }),
    )
    .await
    .session
}

async fn call(r: &Rig, session: Option<&str>, tool: &str, meta: Option<Value>) -> Value {
    let mut params = json!({ "name": tool, "arguments": {} });
    if let Some(m) = meta {
        params["_meta"] = m;
    }
    post(r, session, "tools/call", params).await.body["result"].clone()
}

fn text(res: &Value) -> &str {
    res["content"][0]["text"].as_str().expect("text content")
}

/// The card text the 3a list answer gets on a card-rendering host.
fn short_3a() -> String {
    let summary = mcp_app::card(&fixture("3a")).unwrap().summary.unwrap();
    mcp_app::card_text(&summary, mcp_app::CARD_NOTE).expect("3a has rows")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_renders_the_card_gets_the_card_text() {
    let r = rig().await;

    // A handshake-era host that declares MCP Apps: a session id, and on
    // calls in that session the card text beside the unchanged card data
    // and envelope.
    let session = initialize(&r, ui_capabilities())
        .await
        .expect("a card-rendering client gets a session id");
    let res = call(&r, Some(&session), "list_agents", None).await;
    assert_eq!(text(&res), short_3a(), "{res}");
    assert!(!text(&res).contains("full text of 3a"));
    assert_eq!(res["_meta"][mcp_app::META_KEY], read_json(&golden("3a")));
    // The model-visible typed result does not carry the rows either:
    // ChatGPT shows `structuredContent` to its model, which otherwise pages
    // the tool itself and prints every row under the card. One summary of
    // the card instead, beside the trace id.
    assert_eq!(
        res["structuredContent"]["card"],
        json!({ "title": "6 planned Agents", "rows": 6, "shown_in_card": true }),
        "{res}"
    );
    let mut keys: Vec<&String> = res["structuredContent"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["card", "latency_ms", "trace_id"],
        "nothing beside the summary: {res}"
    );
    // A card without rows (a receipt's facts) keeps the full text, and
    // the full typed result.
    let res = call(&r, Some(&session), "receipt", None).await;
    assert_eq!(text(&res), "full text of 3e", "{res}");
    assert!(
        res["structuredContent"]["result"]["surface"].is_object(),
        "{res}"
    );
    assert!(res["structuredContent"].get("card").is_none(), "{res}");
    // A card cut by its row budget does not show every row: the full text
    // and the full typed result stay.
    let res = call(&r, Some(&session), "cut_list", None).await;
    assert!(text(&res).contains("full text of the cut list"), "{res}");
    assert!(
        res["structuredContent"]["result"]["surface"].is_object(),
        "{res}"
    );
    assert!(res["structuredContent"].get("card").is_none(), "{res}");
    // Prose the card does not show stays: the narration, the chart and the
    // sources follow the card text; the text rendering of the rows does not.
    let res = call(&r, Some(&session), "narrated", None).await;
    assert_eq!(
        text(&res),
        format!(
            "3 Things: A, B, C \u{2014} shown in the card.\n{}\n\n\
             Results exclude the archived system.\n\n\
             ![chart](https://img.example/c.png)\n\n\
             Sources: [Doc](https://docs.example/d)",
            mcp_app::CARD_NOTE
        ),
        "{res}"
    );
    // No card at all: the text.
    let res = call(&r, Some(&session), "prose", None).await;
    assert_eq!(text(&res), "just prose");

    // A 2026-07-28 host declares it per request.
    let modern = |caps: Value| {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "card-text-test", "version": "1.0.0" },
            "io.modelcontextprotocol/clientCapabilities": caps
        })
    };
    let res = call(&r, None, "list_agents", Some(modern(ui_capabilities()))).await;
    assert_eq!(text(&res), short_3a(), "{res}");
    assert!(res["structuredContent"].get("result").is_none(), "{res}");
    // Per request means per request: a 2026-07-28 request without the
    // extension gets the full text, whatever session header it carries.
    let res = call(&r, Some(&session), "list_agents", Some(modern(json!({})))).await;
    assert_eq!(text(&res), "full text of 3a", "{res}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_does_not_render_the_card_keeps_the_full_text() {
    let r = rig().await;

    // No extension declared (Copilot Studio, a plain MCP client): no
    // session id, the full text, and no card messages: a client that cannot
    // render the card does not get the card's data (incl. a producer's
    // data_model) in its result `_meta` either.
    assert_eq!(initialize(&r, json!({})).await, None);
    let res = call(&r, None, "list_agents", None).await;
    assert_eq!(text(&res), "full text of 3a", "{res}");
    assert!(res["_meta"].get(mcp_app::META_KEY).is_none(), "{res}");
    assert!(
        res["structuredContent"]["result"]["surface"].is_object(),
        "a client that does not render the card keeps the full typed result: {res}"
    );

    // The extension without the HTML profile is not this card.
    let other = json!({ "extensions": { UI: { "mimeTypes": ["text/plain"] } } });
    assert_eq!(initialize(&r, other).await, None);

    // A session id Triton did not issue for a card-rendering client.
    let res = call(&r, Some("not-a-triton-session"), "list_agents", None).await;
    assert_eq!(text(&res), "full text of 3a", "{res}");
}

/// An upstream's `model_context` (the ids of the rows it showed) is for the
/// calling model only. A card host's model reads the text content only
/// (MCP Apps: `structuredContent` is not added to model context), and its
/// text is the card text, the row names: without the model context it could
/// not name a row in a follow-up call. Every client gets it after the text;
/// neither the card nor the typed result carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_model_context_reaches_every_model_and_never_the_card() {
    let r = rig().await;
    let refs = "Ids for follow-up calls: Alpha = thing t-alpha; Beta = thing t-beta";

    let session = initialize(&r, ui_capabilities()).await.expect("card host");
    let res = call(&r, Some(&session), "refs_list", None).await;
    assert_eq!(
        text(&res),
        format!(
            "2 Things: Alpha, Beta \u{2014} shown in the card.\n{}\n\n{refs}",
            mcp_app::CARD_NOTE
        ),
        "{res}"
    );
    let card = res["_meta"][mcp_app::META_KEY].to_string();
    assert!(card.contains("Alpha"), "the card shows the rows: {card}");
    assert!(!card.contains("t-alpha"), "never the ids: {card}");
    assert!(
        !res["structuredContent"].to_string().contains("t-alpha"),
        "{res}"
    );

    // A plain client: the full text, then the same context; the typed
    // result keeps everything else.
    let res = call(&r, None, "refs_list", None).await;
    assert_eq!(
        text(&res),
        format!("full text of the refs list\n\n{refs}"),
        "{res}"
    );
    assert!(
        res["structuredContent"]["result"]
            .get("model_context")
            .is_none(),
        "{res}"
    );
    assert!(
        res["structuredContent"]["result"]["surface"].is_object(),
        "{res}"
    );

    // Without one, nothing changes.
    let res = call(&r, Some(&session), "list_agents", None).await;
    assert_eq!(text(&res), short_3a(), "{res}");
}
