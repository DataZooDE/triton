//! Gemini-Enterprise-compatible **A2UI v0.9** builder (a2ui.org, the
//! `application/json+a2ui` wire form that Vertex AI Search / Agentspace
//! actually renders).
//!
//! This is deliberately SEPARATE from the historical `v08`/`v09` builders in
//! this module: those emit Triton's own `{version, stream:[…]}` dialect for
//! the Explorer's Flutter renderer, which no shipping surface consumes. Gemini
//! Enterprise renders the a2ui.org catalog format — a streamed message array
//! (`createSurface` → `updateComponents`) of flat, id-referenced components
//! drawn from the **basic catalog** — which is what this file produces.
//!
//! Contract (github.com/a2ui-project/a2ui `specification/v0_9`, and the
//! Gemini reference agent `wadave/agent-a2ui-demo`):
//!   - `data` is an ARRAY of messages, each stamped `"version":"v0.9"`.
//!   - components use the flat discriminator `{"id","component":"…", …}`.
//!   - a `Card` has ONE `child`; multiple children go through `Column`/`Row`.
//!   - a `Button` label is a child `Text` referenced by id, and its `action`
//!     is REQUIRED — we emit `{"event":{"name","context"}}` so a click posts
//!     an A2UI `action` message back to the agent.
//!   - there is no Chart component: a chart is an `Image` with a URL.
//!
//! Input is the RAW surface JSON the agent produced (`{components:[…]}` with
//! each component tagged by `kind`), not the typed [`super::Surface`], because
//! the embedded agent stamps fields the typed model does not carry (notably a
//! signed `image_url` on a `report`), and the spec-A2A path already treats the
//! surface as raw JSON.

use serde_json::{Value, json};

/// The A2UI v0.9 A2A-extension URI (agent-card advertisement + activation).
pub const EXTENSION_URI: &str = "https://a2ui.org/a2a-extension/a2ui/v0.9";
/// The DataPart MIME type Gemini Enterprise's renderer actually reads.
///
/// The a2ui.org SDK names the two spellings BACKWARDS: the constant it calls
/// `A2UI_MIME_TYPE` (`application/a2ui+json`) is the unshipped *future*
/// spelling that no client reads yet, while `DEPRECATED_A2UI_MIME_TYPE`
/// (`application/json+a2ui`) is what every shipping renderer — Gemini
/// Enterprise included — matches. Emitting the "+a2ui" form makes GE treat the
/// DataPart as opaque and fall back to rendering the TextPart only (a
/// perfectly-formed card shows as text). Verified against the a2ui SDK
/// (`create_a2ui_part`) and Google's own GE integration notes.
pub const MIME: &str = "application/json+a2ui";
/// The basic component catalog id (declared in the card and in `createSurface`).
pub const BASIC_CATALOG: &str = "https://a2ui.org/specification/v0_9/catalogs/basic/catalog.json";

// Surface id MUST be unique per card: Gemini Enterprise keeps surfaces for
// the life of a conversation and rejects a second `createSurface` with an id
// it already has ("Surface <id> already exists"), which would blank every
// card after the first (follow-up turns, button responses). One fresh id per
// build.

/// Build the A2UI v0.9 message array from a raw surface value
/// (`result["surface"]["components"]`). Returns `None` when the value carries
/// no renderable component, so the caller can fall back to a text part.
pub fn build_messages(result: &Value) -> Option<Vec<Value>> {
    let components = result
        .get("surface")
        .and_then(|s| s.get("components"))
        .and_then(Value::as_array)?;

    // Flat component list (id-referenced); `root_children` collects the ids
    // that hang under the top Column, in surface order.
    let surface_id = format!("triton-answer-{}", uuid::Uuid::new_v4());
    let mut flat: Vec<Value> = Vec::new();
    let mut root_children: Vec<String> = Vec::new();
    let mut n = 0usize;
    let id = |prefix: &str, n: &mut usize| {
        *n += 1;
        format!("{prefix}-{n}")
    };

    for c in components {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or_default();
        match kind {
            // Prose (text/narration) is intentionally NOT put in the card:
            // the streamed answer already renders as the message bubble above
            // the card, and duplicating it inside would show the answer twice.
            // The card is the rich WIDGET — chart + actions + sources.
            "text" | "narration" => {}
            // Chart: the embedded agent stamps a signed public `image_url` on
            // the report; the basic catalog has no chart, so it is an Image.
            "report" => {
                if let Some(url) = c.get("image_url").and_then(Value::as_str) {
                    let cid = id("chart", &mut n);
                    let mut img =
                        json!({ "id": cid, "component": "Image", "url": url, "fit": "contain" });
                    if let Some(t) = c.get("title").and_then(Value::as_str) {
                        img["description"] = json!(t);
                    }
                    flat.push(img);
                    root_children.push(cid);
                }
            }
            "button" => {
                let label = c.get("label").and_then(Value::as_str).unwrap_or("Open");
                let tool = c.get("tool").and_then(Value::as_str).unwrap_or_default();
                // A re-ask follow-up carries `args.message` (the question);
                // report/other buttons don't (`render_report` opens a report).
                let question = c
                    .get("args")
                    .and_then(|a| a.get("message"))
                    .and_then(Value::as_str)
                    .filter(|_| tool != "render_report");
                let text_id = id("btn-text", &mut n);
                let btn_id = id("btn", &mut n);
                flat.push(json!({ "id": text_id, "component": "Text", "text": label }));
                // Carry the re-ask question in the event NAME. On click Gemini
                // Enterprise posts the action back with `name` verbatim but
                // DROPS the context (`context:{}`) and does NOT resolve data
                // bindings — observed on the wire — so the name is the only
                // channel that survives. `ask:<question>` is decoded by the
                // inbound A2UI action handler and re-dispatched as a turn;
                // other buttons keep the tool name.
                let event_name = match question {
                    Some(q) => format!("ask:{q}"),
                    None => tool.to_string(),
                };
                flat.push(json!({
                    "id": btn_id,
                    "component": "Button",
                    "child": text_id,
                    "variant": if c.get("primary").and_then(Value::as_bool) == Some(true) { "primary" } else { "default" },
                    "action": { "event": { "name": event_name, "context": {} } },
                }));
                root_children.push(btn_id);
            }
            // Sources: GE's basic catalog has no link component and rejects a
            // `functionCall: openUrl` Button action ("Validation failed for
            // component 'Button'"), so render each source as a plain Text line
            // ("Source: <label> — <url>"). Not clickable, but the reference is
            // conveyed and the card validates. (http/https only; a ui:// MCP
            // resource can't open in GE and is skipped.)
            "sources" => {
                if let Some(items) = c.get("items").and_then(Value::as_array) {
                    for it in items {
                        let label = it.get("label").and_then(Value::as_str).unwrap_or("Source");
                        let Some(url) = it
                            .get("resource")
                            .and_then(Value::as_str)
                            .filter(|u| u.starts_with("http"))
                        else {
                            continue;
                        };
                        let cid = id("src", &mut n);
                        flat.push(json!({
                            "id": cid,
                            "component": "Text",
                            "text": format!("Source: {label} — {url}"),
                            "variant": "caption",
                        }));
                        root_children.push(cid);
                    }
                }
            }
            _ => {}
        }
    }

    if root_children.is_empty() {
        return None;
    }

    // Top: Card → Column(root_children).
    flat.push(json!({ "id": "root-col", "component": "Column", "children": root_children }));
    flat.push(json!({ "id": "root", "component": "Card", "child": "root-col" }));

    Some(vec![
        json!({
            "version": "v0.9",
            "createSurface": { "surfaceId": surface_id, "catalogId": BASIC_CATALOG },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ])
}

/// Wrap each A2UI message as its OWN A2A `DataPart`.
///
/// A2A 0.3.0 types `DataPart.data` as a JSON **object**, so the A2UI message
/// stream (an array) cannot ride in a single part's `data` — a strict client
/// (Gemini Enterprise's a2a-python SDK) rejects `data: [...]` with
/// `dict_type`. One message per DataPart keeps every `data` a dict and matches
/// ADK's `create_a2ui_part` (one payload dict → one part); the receiver
/// processes the parts in order to rebuild the stream.
pub fn data_parts(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .map(|m| json!({ "kind": "data", "data": m, "metadata": { "mimeType": MIME } }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find<'a>(comps: &'a [Value], id: &str) -> &'a Value {
        comps.iter().find(|c| c["id"] == id).expect("component id")
    }

    #[test]
    fn maps_text_chart_button_and_sources() {
        let result = json!({ "surface": { "components": [
            { "kind": "text", "value": "Initech leads at $2,500.75." },
            { "kind": "report", "report_id": "sales", "image_url": "https://agent-lab.data-zoo.de/report/img/tok", "title": "Sales" },
            { "kind": "button", "label": "What does Initech buy?", "tool": "assistant", "args": { "message": "What does Initech buy?" } },
            { "kind": "sources", "items": [
                { "label": "sales-by-customer", "resource": "https://agent-lab.data-zoo.de/docs/tok" },
                { "label": "ui-only", "resource": "ui://peacock/x" }
            ] }
        ] } });

        let msgs = build_messages(&result).expect("renderable");
        assert_eq!(msgs.len(), 2, "createSurface + updateComponents");
        assert_eq!(msgs[0]["version"], "v0.9");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();

        // Root is a Card whose single child is a Column.
        let root = find(comps, "root");
        assert_eq!(root["component"], "Card");
        let col = find(comps, root["child"].as_str().unwrap());
        assert_eq!(col["component"], "Column");

        // Exactly one Image, carrying the signed chart URL (charts = Image).
        let img = comps
            .iter()
            .find(|c| c["component"] == "Image")
            .expect("image");
        assert_eq!(img["url"], "https://agent-lab.data-zoo.de/report/img/tok");

        // The follow-up button re-invokes its tool via a server event, and its
        // label rides as a child Text (never inline).
        let btn = comps
            .iter()
            .find(|c| {
                c["component"] == "Button"
                    && c["action"]["event"]["name"]
                        .as_str()
                        .map(|n| n.starts_with("ask:"))
                        == Some(true)
            })
            .expect("event button");
        // The re-ask question rides the event NAME as `ask:<question>` (GE
        // drops the action context on click but echoes the name verbatim).
        assert_eq!(btn["action"]["event"]["name"], "ask:What does Initech buy?");
        assert!(
            btn["action"]["event"]["context"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        let btn_text = find(comps, btn["child"].as_str().unwrap());
        assert_eq!(btn_text["text"], "What does Initech buy?");

        // Prose is NOT duplicated inside the card (the bubble shows it).
        assert!(
            !comps
                .iter()
                .any(|c| c["component"] == "Text" && c["text"] == "Initech leads at $2,500.75."),
            "card must not repeat the answer prose"
        );

        // http source → plain Text line (GE rejects functionCall:openUrl);
        // ui:// source is dropped. No Button carries a functionCall action.
        let src = comps
            .iter()
            .find(|c| {
                c["component"] == "Text" && c["text"].as_str().unwrap_or("").starts_with("Source:")
            })
            .expect("source text line");
        assert!(
            src["text"]
                .as_str()
                .unwrap()
                .contains("https://agent-lab.data-zoo.de/docs/tok"),
            "{src}"
        );
        assert!(
            !comps
                .iter()
                .any(|c| c["action"]["functionCall"].is_object()),
            "no functionCall actions (GE rejects them)"
        );
    }

    #[test]
    fn no_renderable_components_yields_none() {
        assert!(build_messages(&json!({ "text": "plain" })).is_none());
        assert!(build_messages(&json!({ "surface": { "components": [] } })).is_none());
        // A report with no image_url has nothing to draw.
        assert!(
            build_messages(&json!({ "surface": { "components": [
            { "kind": "report", "report_id": "x" }
        ] } }))
            .is_none()
        );
    }

    #[test]
    fn data_parts_are_one_dict_each_with_the_ge_mime() {
        let ps = data_parts(vec![
            json!({"version":"v0.9","a":1}),
            json!({"version":"v0.9","b":2}),
        ]);
        assert_eq!(ps.len(), 2, "one DataPart per A2UI message");
        for p in &ps {
            assert_eq!(p["kind"], "data");
            assert_eq!(p["metadata"]["mimeType"], "application/json+a2ui");
            // A2A DataPart.data MUST be an object, never an array.
            assert!(p["data"].is_object(), "DataPart.data must be a dict: {p}");
        }
    }
}
