//! Gemini-Enterprise-compatible **A2UI v0.9** builder (a2ui.org, the
//! `application/json+a2ui` wire form that Vertex AI Search / Agentspace
//! actually renders).
//!
//! This is deliberately SEPARATE from the historical `v08`/`v09` builders in
//! this module: those emit Triton's own `{version, stream:[…]}` dialect for
//! the Explorer's Flutter renderer, which no shipping surface consumes. Gemini
//! Enterprise renders the a2ui.org catalog format — a streamed message array
//! (`createSurface` → `updateComponents`) of flat, id-referenced components
//! drawn from GE's **composite (Material) catalog** — which is what this file
//! produces.
//!
//! Contract (github.com/a2ui-project/a2ui `specification/v0_9`, Google's GE
//! A2UI component-gallery reference, and the a2ui-project GE sample):
//!   - `data` is an ARRAY of messages, each stamped `"version":"v0.9"`.
//!   - components use the flat discriminator `{"id","component":"…", …}`.
//!   - we render with GE's **composite catalog** (Material Design components),
//!     so the card is `MaterialCard`/`MaterialColumn`/`MaterialRow` and the
//!     widgets are `MaterialButton`/`MaterialImage` — real Material, not plain
//!     primitives. A `MaterialCard` takes a `children` LIST (not a single
//!     `child`).
//!   - a `MaterialButton` carries its `label` DIRECTLY (no child Text) plus a
//!     `variant`/`color`; its `action` is REQUIRED — we emit
//!     `{"event":{"name","context"}}` so a click posts an A2UI `action` back.
//!   - charts render as a native `VegaChart` (Vega-Lite `spec`, interactive)
//!     when the A2A layer injects one from peacock; else a static `Image` URL.
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
/// The catalog id declared in the card and in `createSurface`.
///
/// Gemini Enterprise's own **composite catalog** (Material Design components +
/// the basic primitives + GE-native ones), NOT the design-agnostic a2ui.org
/// `basic` catalog. Using it lets the card render as real Material — themed
/// `MaterialButton`/`MaterialText`/`MaterialCard` instead of plain primitives.
/// (Authoritative id per Google's GE A2UI reference + the a2ui-project GE
/// sample catalog's own `$id`.)
pub const BASIC_CATALOG: &str =
    "https://www.gstatic.com/vertexaisearch/a2ui/v0_9/gemini_enterprise_composite_catalog.json";
/// Surface theme `primaryColor` (`^#[0-9a-fA-F]{6}$`) sent in `createSurface`.
/// Gemini brand blue — the same `--pk-brand` the peacock `gemini.css` host theme
/// gives the charts — so the Material card's primary accents match the chart.
const THEME_PRIMARY_COLOR: &str = "#1a73e8";

/// Sentinel stored as a source button's re-ask "question": clicking the button
/// opens the cited document in a Canvas side panel rather than dispatching a
/// turn to the agent. [`question_for`] returns this string like any re-ask; the
/// A2A layer detects it via [`open_doc`] and replies with
/// [`build_document_canvas`] instead of running the agent. The prefix is a
/// control char so it can never collide with a question a user could type.
const DOC_OPEN_SENTINEL: &str = "\u{1}a2ui-open-doc\u{1}";

/// Feature flag (`TRITON_GE_DOC_CANVAS`): emit in-card "Open source" buttons
/// that render the cited document in a GE **Canvas** side panel (an `IFrameUrl`
/// to the signed `/docs` page). OFF by default — the `IFrameUrl` host must
/// first be on GE's widget allowlist, otherwise the panel shows a security
/// error. When off, `sources` stay a no-op here and ride the prose bubble as
/// Markdown links (`reply_text`), exactly as before.
pub fn doc_canvas_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRITON_GE_DOC_CANVAS")
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// If `text` is a source-open sentinel (produced by a card source button and
/// round-tripped through [`question_for`]), return `(label, url)` for the
/// document to open. `None` for an ordinary re-ask or text turn.
pub fn open_doc(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix(DOC_OPEN_SENTINEL)?;
    let (label, url) = rest.split_once('\u{1}')?;
    let url = url.trim();
    (!url.is_empty()).then_some((label, url))
}

/// Fixed surfaceId prefix; a v4 UUID follows. The id is kept SHORT
/// (`triton-answer-<uuid>`, 50 chars) on purpose: Gemini Enterprise TRUNCATES
/// the surfaceId to the uuid on a button click (observed on the wire — a longer
/// encoded id came back stripped), so it cannot carry a payload. The uuid does
/// survive, so it keys the per-card re-ask table below.
const SURFACE_PREFIX: &str = "triton-answer-";

/// Bounded, process-wide map: `surfaceId` → { button id number → re-ask
/// question }. GE strips name/context on click and truncates the surfaceId to
/// the uuid, but PRESERVES the component id — so the recoverable pair is
/// `(surfaceId-uuid, our btn-N)`, which this table maps back to the question.
///
/// This is process-local, so `dz-agent-template` runs a single replica in lab
/// (values-lab `replicaCount: 1`); a multi-replica deployment would need a
/// shared store keyed by the same surfaceId. FIFO-capped at [`MAX_SURFACES`];
/// a stale miss falls through to the turn's placeholder text (no crash).
static SURFACE_QUESTIONS: std::sync::Mutex<
    Vec<(String, std::collections::HashMap<usize, String>)>,
> = std::sync::Mutex::new(Vec::new());
const MAX_SURFACES: usize = 512;

fn remember_surface(surface_id: &str, qmap: std::collections::HashMap<usize, String>) {
    if qmap.is_empty() {
        return;
    }
    if let Ok(mut store) = SURFACE_QUESTIONS.lock() {
        store.push((surface_id.to_string(), qmap));
        let overflow = store.len().saturating_sub(MAX_SURFACES);
        if overflow > 0 {
            store.drain(0..overflow);
        }
    }
}

/// Resolve a GE button click to the re-ask question it should run.
/// `source_component_id` is GE's echoed id (`"btn-N"`); its trailing number
/// keys the per-card table recorded at build time under `surface_id`. `None` ⇒
/// not a re-ask (→ caller falls back to the turn's text).
pub fn question_for(surface_id: &str, source_component_id: &str) -> Option<String> {
    let n: usize = source_component_id.rsplit('-').next()?.parse().ok()?;
    let store = SURFACE_QUESTIONS.lock().ok()?;
    let hit = store.iter().rev().find(|(sid, _)| sid == surface_id);
    hit.and_then(|(_, m)| m.get(&n).cloned())
}

/// GE-specific presentation tweaks to peacock's Vega-Lite spec (on a clone —
/// peacock's shared spec is untouched). GE's `VegaChart` renders the raw spec
/// as-is, which looks cluttered next to the Teams card: peacock rotates the
/// x-axis labels vertical, and a single-series bar colored by its own x-field
/// carries a redundant color legend. Fix both, and let the chart fill GE's
/// iframe width.
fn beautify_vega_for_ge(spec: &Value) -> Value {
    let mut s = spec.clone();
    if let Some(obj) = s.as_object_mut() {
        // Responsive: fill the iframe rather than a fixed peacock width.
        obj.insert("width".to_string(), json!("container"));
        if let Some(enc) = obj.get_mut("encoding").and_then(Value::as_object_mut) {
            let x_field = enc
                .get("x")
                .and_then(|x| x.get("field"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Horizontal x-axis labels (peacock rotates them vertical, which is
            // unreadable in GE's renderer).
            if let Some(x) = enc.get_mut("x").and_then(Value::as_object_mut) {
                match x.get_mut("axis").and_then(Value::as_object_mut) {
                    Some(a) => {
                        a.insert("labelAngle".to_string(), json!(0));
                    }
                    None => {
                        x.insert("axis".to_string(), json!({ "labelAngle": 0 }));
                    }
                }
            }
            // Drop the redundant color legend when `color` encodes the SAME
            // field as `x` (e.g. supplier-reliability colors each bar by the
            // supplier the x-axis already labels). A genuine series (a stacked
            // bar colored by a different field) keeps its legend.
            if let (Some(xf), Some(color)) =
                (x_field, enc.get_mut("color").and_then(Value::as_object_mut))
                && color.get("field").and_then(Value::as_str) == Some(xf.as_str())
            {
                color.insert("legend".to_string(), Value::Null);
            }
        }
    }
    s
}

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
    let mut flat: Vec<Value> = Vec::new();
    let mut root_children: Vec<String> = Vec::new();
    // Buttons are collected here and laid out in a single horizontal Row (chips)
    // rather than stacked full-width down the Column — matching the Teams/Chat
    // action-chip row. (Sources are not in the card — see the `sources` arm.)
    let mut button_ids: Vec<String> = Vec::new();
    // Predicted-GE-id → re-ask question, for this card's follow-up buttons.
    let mut qmap: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
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
            // Chart. Prefer a NATIVE interactive `VegaChart` when a Vega-Lite
            // `vega_spec` is present (the A2A layer expands the report via
            // `render_report` and injects peacock's spec) — GE renders it with
            // tooltips/hover. Otherwise fall back to the static PNG as a basic
            // `Image` (NOT `MaterialImage`: GE's Material renderer left the
            // signed URL as a broken/empty <img>; basic Image loads + self-sizes.
            // Mixing basic + Material is fine — the composite catalog has both).
            "report" => {
                if let Some(spec) = c.get("vega_spec").filter(|s| s.is_object()) {
                    let cid = id("chart", &mut n);
                    flat.push(json!({
                        "id": cid,
                        "component": "VegaChart",
                        "spec": beautify_vega_for_ge(spec),
                        "height": 300,
                    }));
                    root_children.push(cid);
                } else if let Some(url) = c.get("image_url").and_then(Value::as_str) {
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
                // Re-ask follow-ups carry the question in `args.question`
                // (agent-core `AssistantTool`); accept `args.message` too for
                // other producers. `render_report` opens a report, not a re-ask.
                let question = c
                    .get("args")
                    .and_then(|a| a.get("question").or_else(|| a.get("message")))
                    .and_then(Value::as_str)
                    .filter(|_| tool != "render_report");
                // MaterialButton takes its `label` directly (no child Text), so
                // one id per button. GE strips our event name/context and
                // truncates the surfaceId to its uuid on click, but PRESERVES the
                // component id — a click echoes `sourceComponentId` equal to the
                // Button's own `id` (`btn-N`). `n` here is exactly that N, so key
                // `N → question` for the inbound re-ask lookup (report/other
                // buttons carry no question and are not recorded).
                let btn_id = id("btn", &mut n);
                if let Some(q) = question {
                    qmap.insert(n, q.to_string());
                }
                // `flat` (Material filled, solid) + `color:primary` (picks up the
                // surface theme color) → solid buttons with white text, uniform
                // across the row.
                flat.push(json!({
                    "id": btn_id.clone(),
                    "component": "MaterialButton",
                    "label": label,
                    "variant": "flat",
                    "color": "primary",
                    "action": { "event": { "name": tool, "context": {} } },
                }));
                button_ids.push(btn_id);
            }
            // Sources: with the Canvas feature OFF (default) they ride the
            // prose bubble as Markdown links (`reply_text`) — GE's basic
            // catalog can't hyperlink in-card, so this stays a no-op. With it
            // ON (allowlist arranged), each https source becomes an OUTLINED
            // "Open" button; clicking it opens the signed `/docs` page in a
            // Canvas side panel via the [`open_doc`] round-trip (keyed by the
            // button's own id, same machinery as a re-ask).
            "sources" => {
                if !doc_canvas_enabled() {
                    continue;
                }
                let Some(items) = c.get("items").and_then(Value::as_array) else {
                    continue;
                };
                for it in items {
                    let Some(url) = it
                        .get("resource")
                        .and_then(Value::as_str)
                        .filter(|u| u.starts_with("https://"))
                    else {
                        continue;
                    };
                    let label = it.get("label").and_then(Value::as_str).unwrap_or("source");
                    let btn_id = id("btn", &mut n);
                    // Round-trip payload: the click resolves to this sentinel,
                    // which the A2A layer turns into a Canvas (no agent turn).
                    qmap.insert(n, format!("{DOC_OPEN_SENTINEL}{label}\u{1}{url}"));
                    // Outlined (not filled) so a source reads as a secondary,
                    // link-like action, distinct from the solid follow-ups.
                    flat.push(json!({
                        "id": btn_id.clone(),
                        "component": "MaterialButton",
                        "label": format!("Open: {label}"),
                        "variant": "outlined",
                        "color": "primary",
                        "action": { "event": { "name": "open_doc", "context": {} } },
                    }));
                    button_ids.push(btn_id);
                }
            }
            _ => {}
        }
    }

    // Follow-up/report buttons → a small grid: MaterialRow has no wrap and no
    // gap, so cramming 4 buttons in one row squeezes them and long labels wrap
    // inside a fixed-height pill (visible "overprinting"). Chunk into rows of
    // two so each button gets room; the rows stack in the outer Column.
    for (k, chunk) in button_ids.chunks(2).enumerate() {
        let row_id = format!("btn-row-{k}");
        flat.push(json!({
            "id": row_id,
            "component": "MaterialRow",
            "children": chunk,
            "justify": "start",
            "align": "stretch",
        }));
        root_children.push(row_id);
    }

    if root_children.is_empty() {
        return None;
    }

    // Top: MaterialCard(outlined) → MaterialColumn(root_children).
    flat.push(
        json!({ "id": "root-col", "component": "MaterialColumn", "children": root_children }),
    );
    flat.push(json!({
        "id": "root",
        "component": "MaterialCard",
        "appearance": "outlined",
        "children": ["root-col"],
    }));

    // Short, unique surfaceId (GE truncates anything longer to the uuid), and
    // record this card's re-ask table under it for the click round-trip.
    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    remember_surface(&surface_id, qmap);

    Some(vec![
        json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": surface_id,
                "catalogId": BASIC_CATALOG,
                // v0.9 styling: `createSurface.theme` (which replaced v0.8's
                // `styles`) carries theme params the Material renderer applies —
                // GE uses `primaryColor` for primary buttons and active borders.
                // We set the DataZoo/Gemini brand blue so the card's accents
                // match the chart palette (peacock `gemini.css` `--pk-brand`),
                // and name the agent beside the surface. Renderers ignore fields
                // they don't use, so this is safe on every A2UI host.
                "theme": {
                    "primaryColor": THEME_PRIMARY_COLOR,
                    "agentDisplayName": "DataZoo Agent",
                },
            },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ])
}

/// Build a **Canvas-rooted** A2UI surface that shows `url` (the signed,
/// Material-themed `/docs` page) in Gemini Enterprise's resizable **side
/// panel**. `Canvas` MUST be the surface root (GE renders a compact opener
/// card in the chat stream that opens the panel — `autoOpen` opens it
/// immediately). The `IFrameUrl` host must be on GE's widget allowlist, which
/// is why the source buttons that reach here are gated by
/// [`doc_canvas_enabled`]. Returns the `createSurface` + `updateComponents`
/// message pair, ready for [`data_parts`].
pub fn build_document_canvas(url: &str, title: &str) -> Vec<Value> {
    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
    let flat = vec![
        json!({
            "id": "doc-frame",
            "component": "IFrameUrl",
            "url": url,
            "title": title,
            "height": 640,
        }),
        json!({
            "id": "root",
            "component": "Canvas",
            "children": ["doc-frame"],
            "autoOpen": true,
            "cardTitle": title,
            "cardDescription": "Open the source document",
            "cardIcon": "description",
        }),
    ];
    vec![
        json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": surface_id,
                "catalogId": BASIC_CATALOG,
                "theme": {
                    "primaryColor": THEME_PRIMARY_COLOR,
                    "agentDisplayName": "DataZoo Agent",
                },
            },
        }),
        json!({
            "version": "v0.9",
            "updateComponents": { "surfaceId": surface_id, "components": flat },
        }),
    ]
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
            { "kind": "button", "label": "What does Initech buy?", "tool": "assistant", "args": { "question": "What does Initech buy?" } },
            { "kind": "sources", "items": [
                { "label": "sales-by-customer", "resource": "https://agent-lab.data-zoo.de/docs/tok" },
                { "label": "ui-only", "resource": "ui://peacock/x" }
            ] }
        ] } });

        let msgs = build_messages(&result).expect("renderable");
        assert_eq!(msgs.len(), 2, "createSurface + updateComponents");
        assert_eq!(msgs[0]["version"], "v0.9");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        // createSurface carries a v0.9 theme so GE styles the card (Material
        // primary accents) with the brand color.
        assert_eq!(
            msgs[0]["createSurface"]["theme"]["primaryColor"],
            THEME_PRIMARY_COLOR
        );
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();

        // Root is a MaterialCard whose single child (a LIST) is a MaterialColumn.
        let root = find(comps, "root");
        assert_eq!(root["component"], "MaterialCard");
        let col_id = root["children"][0].as_str().unwrap();
        let col = find(comps, col_id);
        assert_eq!(col["component"], "MaterialColumn");

        // Exactly one Image (basic — GE's MaterialImage won't load the signed
        // URL), carrying the signed chart URL.
        let img = comps
            .iter()
            .find(|c| c["component"] == "Image")
            .expect("image");
        assert_eq!(img["url"], "https://agent-lab.data-zoo.de/report/img/tok");

        // The re-ask MaterialButton carries its label directly (no child Text).
        // GE strips our name/context on click but PRESERVES the component id, so
        // the question is recovered via the per-surface table keyed by the
        // button's own id number (`surfaceId` + `btn-N`).
        let btn = comps
            .iter()
            .find(|c| {
                c["component"] == "MaterialButton" && c["action"]["event"]["name"] == "assistant"
            })
            .expect("re-ask button");
        assert_eq!(btn["label"], "What does Initech buy?");
        assert_eq!(btn["variant"], "flat");
        assert_eq!(btn["color"], "primary");
        // GE echoes the Button's own id on click. This surface: chart=1,
        // button=2 (MaterialButton needs no child Text) — so `btn-2` resolves.
        let sid = msgs[0]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_eq!(btn["id"], "btn-2");
        assert_eq!(
            question_for(sid, "btn-2").as_deref(),
            Some("What does Initech buy?"),
        );
        // A wrong id / unknown surface resolves to nothing.
        assert_eq!(question_for(sid, "btn-99"), None);
        assert_eq!(question_for("nope", "btn-2"), None);

        // Buttons are laid out in MaterialRows (a grid, ≤2 per row), not
        // stacked as direct Column children.
        let row = find(comps, "btn-row-0");
        assert_eq!(row["component"], "MaterialRow");
        assert!(
            row["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "btn-2"),
            "button must live inside the Row: {row}"
        );
        assert_eq!(
            col["children"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| *c == "btn-2"),
            None,
            "button must NOT be a direct Column child"
        );

        // Prose is NOT duplicated inside the card (the bubble shows it).
        assert!(
            !comps.iter().any(|c| {
                (c["component"] == "Text" || c["component"] == "MaterialText")
                    && c["text"] == "Initech leads at $2,500.75."
            }),
            "card must not repeat the answer prose"
        );

        // Sources are NOT in the card (GE can't hyperlink there); they ride the
        // prose bubble as Markdown links via reply_text. So the card has no
        // "sources" component and no source Text line at all.
        assert!(
            !comps.iter().any(|c| c["id"] == "sources"),
            "card must not carry a sources component"
        );
        assert!(
            !comps
                .iter()
                .any(|c| c["component"] == "Text"
                    && c["text"].as_str().unwrap_or("").contains("Source")),
            "card must not carry a source text line"
        );
        assert!(
            !comps
                .iter()
                .any(|c| c["action"]["functionCall"].is_object()),
            "no functionCall actions (GE rejects them)"
        );
    }

    #[test]
    fn report_with_vega_spec_emits_native_vegachart() {
        // When the report carries a Vega-Lite `vega_spec` (injected by the A2A
        // layer from peacock's render_report), GE gets a native interactive
        // VegaChart, not the static Image.
        let spec = json!({
            "mark": "bar",
            "data": { "values": [{ "c": "widgets", "v": 900 }] },
            // color == x → the legend is redundant and must be dropped for GE.
            "encoding": {
                "x": { "field": "c" },
                "y": { "field": "v" },
                "color": { "field": "c" },
            },
        });
        let result = json!({ "surface": { "components": [
            { "kind": "report", "report_id": "sales", "image_url": "https://x/img/tok", "vega_spec": spec },
        ] } });
        let msgs = build_messages(&result).expect("renderable");
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        let vega = comps
            .iter()
            .find(|c| c["component"] == "VegaChart")
            .expect("VegaChart");
        assert_eq!(vega["spec"]["mark"], "bar");
        // GE presentation tweaks: horizontal x labels, redundant legend dropped,
        // responsive width.
        assert_eq!(vega["spec"]["encoding"]["x"]["axis"]["labelAngle"], 0);
        assert!(vega["spec"]["encoding"]["color"]["legend"].is_null());
        assert_eq!(vega["spec"]["width"], "container");
        // Static Image must NOT also be emitted (no double chart).
        assert!(!comps.iter().any(|c| c["component"] == "Image"));
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
    fn build_document_canvas_is_canvas_rooted_with_an_iframe() {
        let msgs =
            build_document_canvas("https://agent-lab.data-zoo.de/docs/tok", "Beverages GmbH");
        assert_eq!(msgs.len(), 2, "createSurface + updateComponents");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        // Root MUST be the Canvas (GE requires it as the surface root), and it
        // holds the IFrameUrl pointing at the signed /docs page.
        let root = find(comps, "root");
        assert_eq!(root["component"], "Canvas");
        assert_eq!(root["autoOpen"], true);
        assert_eq!(root["cardTitle"], "Beverages GmbH");
        let child = root["children"][0].as_str().unwrap();
        let frame = find(comps, child);
        assert_eq!(frame["component"], "IFrameUrl");
        assert_eq!(frame["url"], "https://agent-lab.data-zoo.de/docs/tok");
    }

    #[test]
    fn open_doc_round_trips_the_sentinel() {
        // What a source button stores in the re-ask table, and what the A2A
        // layer parses back out of the resolved question.
        let stored = format!("{DOC_OPEN_SENTINEL}Beverages GmbH\u{1}https://x/docs/tok");
        assert_eq!(
            open_doc(&stored),
            Some(("Beverages GmbH", "https://x/docs/tok"))
        );
        // An ordinary re-ask question is not an open-doc sentinel.
        assert_eq!(open_doc("What does Initech buy?"), None);
    }

    #[test]
    fn sources_stay_out_of_the_card_when_the_canvas_flag_is_off() {
        // Default (flag off): a sources-only surface renders nothing in-card —
        // sources ride the prose bubble as Markdown links, unchanged. The
        // flag-ON rendering (source → outlined Open button → Canvas) is
        // verified live in the GE UI (the OnceLock-cached env flag can't be
        // toggled per-test in a shared process).
        let result = json!({ "surface": { "components": [
            { "kind": "sources", "items": [
                { "label": "sales-by-customer", "resource": "https://agent-lab.data-zoo.de/docs/tok" }
            ] }
        ] } });
        assert!(
            !doc_canvas_enabled(),
            "flag defaults off in the test process"
        );
        assert!(
            build_messages(&result).is_none(),
            "with the canvas flag off, a sources-only surface renders nothing in-card"
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
