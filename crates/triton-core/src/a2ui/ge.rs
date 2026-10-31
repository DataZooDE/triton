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
/// opens the cited document inline rather than dispatching a turn to the agent.
/// [`question_for`] returns this string like any re-ask; the A2A layer detects
/// it via [`open_doc`] and replies with the document surface
/// ([`build_document_material`], or [`build_document_canvas`] under
/// [`doc_iframe_enabled`]) instead of running the agent. The prefix is a
/// control char so it can never collide with a question a user could type.
const DOC_OPEN_SENTINEL: &str = "\u{1}a2ui-open-doc\u{1}";

/// Feature flag (`TRITON_GE_DOC_CANVAS`): emit in-card "Open source" buttons
/// that open the cited document inline in GE. OFF by default; when off,
/// `sources` stay a no-op here and ride the prose bubble as Markdown links
/// (`reply_text`), exactly as before. The RENDERING a click produces is the
/// **native Material** document view ([`build_document_material`], GA
/// composite-catalog widgets, no allowlist) unless [`doc_iframe_enabled`].
pub fn doc_canvas_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRITON_GE_DOC_CANVAS")
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// Feature flag (`TRITON_GE_DOC_IFRAME`): opt into the richer **Canvas side
/// panel + `IFrameUrl`** rendering (the exact Material `/docs` page) instead of
/// the native-Material default. OFF by default because `IFrameUrl`/`Canvas` are
/// composite-catalog components Google has not documented, and the `IFrameUrl`
/// host must be on GE's widget allowlist (a Google-side request) or the panel
/// shows a security error. Turn on ONLY once that allowlist is confirmed.
pub fn doc_iframe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRITON_GE_DOC_IFRAME")
            .or_else(|_| std::env::var("TRITON_GE_DOC_CANVAS"))
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// A source's document reference, recovered from the open-doc sentinel a card
/// button round-tripped through [`question_for`]. `skill`/`id` drive the
/// native Material render (a `render_report(document,…)` dispatch); `url` is
/// the signed `/docs` page for the iframe variant. All borrow from the input.
pub struct OpenDocRef<'a> {
    pub label: &'a str,
    pub skill: &'a str,
    pub id: &'a str,
    pub url: &'a str,
}

/// Parse the open-doc sentinel (`label \u1 skill \u1 id \u1 url`). `None` for
/// an ordinary re-ask or text turn, or when neither a `(skill,id)` pair nor a
/// `url` is recoverable (nothing to open).
pub fn open_doc(text: &str) -> Option<OpenDocRef<'_>> {
    let rest = text.strip_prefix(DOC_OPEN_SENTINEL)?;
    let mut parts = rest.splitn(4, '\u{1}');
    let label = parts.next().unwrap_or("");
    let skill = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let url = parts.next().unwrap_or("").trim();
    // Defence in depth: Triton only ever mints https document URLs.
    if !url.is_empty() && !url.starts_with("https://") {
        return None;
    }
    let has_ref = (!skill.is_empty() && !id.is_empty()) || !url.is_empty();
    has_ref.then_some(OpenDocRef {
        label,
        skill,
        id,
        url,
    })
}

/// Split a source's `"skill \u{b7} id"` label (report.rs) back into its parts.
fn split_skill_id(label: &str) -> (&str, &str) {
    match label.split_once(" \u{b7} ") {
        Some((s, i)) => (s.trim(), i.trim()),
        None => ("", ""),
    }
}

/// Fixed surfaceId prefix; a v4 UUID follows. The id is kept SHORT
/// (`triton-answer-<uuid>`, 50 chars) on purpose: Gemini Enterprise TRUNCATES
/// the surfaceId to the uuid on a button click (observed on the wire — a longer
/// encoded id came back stripped), so it cannot carry a payload. The uuid does
/// survive, so it keys the per-card re-ask table below.
const SURFACE_PREFIX: &str = "triton-answer-";
/// Producer-built (passthrough) surfaces get their own prefix, so a click on
/// one never falls through to the numeric `btn-N` table of Triton's cards.
const PASSTHROUGH_PREFIX: &str = "triton-a2ui-";

/// Bounded, process-wide map: `surfaceId` → { button id number → re-ask
/// question }. A click echoes `surfaceId` and the component id (`btn-N`), so
/// the pair maps back to the question. (Measured 2026-10-31: GE also keeps the
/// event `name` and resolves data bindings in `context`; Triton's own buttons
/// send `context: {}`, so only the id pair carries anything for them.)
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

/// Producer-built surfaces' re-ask tables, keyed by the exact component id
/// (a producer names its own components; Triton's numeric `btn-N` convention
/// does not apply). Same FIFO cap as [`SURFACE_QUESTIONS`].
static PASSTHROUGH_ACTIONS: std::sync::Mutex<
    Vec<(String, std::collections::HashMap<String, String>)>,
> = std::sync::Mutex::new(Vec::new());

fn remember_actions(surface_id: &str, actions: std::collections::HashMap<String, String>) {
    if actions.is_empty() {
        return;
    }
    if let Ok(mut store) = PASSTHROUGH_ACTIONS.lock() {
        store.push((surface_id.to_string(), actions));
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
    // A producer-built surface: the exact component id, nothing else (an
    // evicted or unknown surface resolves to nothing).
    if surface_id.starts_with(PASSTHROUGH_PREFIX) {
        let store = PASSTHROUGH_ACTIONS.lock().ok()?;
        let (_, actions) = store.iter().rev().find(|(sid, _)| sid == surface_id)?;
        return actions.get(source_component_id).cloned();
    }
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

/// [`build_messages`] for a request that did or did not activate the A2UI
/// extension: producer-built surfaces (`surface.a2ui`) only go to a client
/// that negotiated A2UI.
pub fn build_messages_negotiated(result: &Value, a2ui: bool) -> Option<Vec<Value>> {
    if a2ui || result.get("surface").and_then(|s| s.get("a2ui")).is_none() {
        return build_messages(result);
    }
    let mut without = result.clone();
    if let Some(surface) = without.get_mut("surface").and_then(Value::as_object_mut) {
        surface.remove("a2ui");
    }
    build_messages(&without)
}

/// Build the A2UI v0.9 message array from a raw surface value: the
/// producer-built surfaces of `surface.a2ui` first (verbatim, see
/// [`passthrough_surfaces`]), then Triton's own card from
/// `surface.components`. Returns `None` when neither yields a surface, so the
/// caller can fall back to a text part.
pub fn build_messages(result: &Value) -> Option<Vec<Value>> {
    let surface = result.get("surface")?;
    let empty = Vec::new();
    let components = surface
        .get("components")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    // Flat component list (id-referenced); `root_children` collects the ids
    // that hang under the top Column, in surface order.
    let mut flat: Vec<Value> = Vec::new();
    let mut root_children: Vec<String> = Vec::new();
    // Action buttons (re-asks, follow-ups) and source buttons are collected
    // separately so secondary document buttons never crowd out or misalign
    // the primary action chips.
    let mut action_btn_ids: Vec<String> = Vec::new();
    let mut source_btn_ids: Vec<String> = Vec::new();
    // Predicted-GE-id → re-ask question, for this card's follow-up buttons.
    let mut qmap: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    // Producers that speak A2UI v0.9 themselves put their surfaces in
    // `surface.a2ui` — a sibling of `components`, so the typed component
    // vocabulary (and every channel built on it) never sees them. They go out
    // first, ahead of the card Triton builds below.
    let passthrough = passthrough_surfaces(surface.get("a2ui"));
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
                // one id per button. A click echoes `sourceComponentId` equal to
                // the Button's own `id` (`btn-N`); our context is empty, so the
                // id is the only payload. `n` here is exactly that N, so key
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
                action_btn_ids.push(btn_id);
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
                    let (skill, doc_id) = split_skill_id(label);
                    let btn_id = id("btn", &mut n);
                    // Round-trip payload: the click resolves to this sentinel,
                    // which the A2A layer renders (native Material by default,
                    // or the /docs iframe when TRITON_GE_DOC_IFRAME) — no agent
                    // turn. Carries skill/id (for the render dispatch) + url.
                    qmap.insert(
                        n,
                        format!("{DOC_OPEN_SENTINEL}{label}\u{1}{skill}\u{1}{doc_id}\u{1}{url}"),
                    );
                    // Shorten long labels cleanly so they don't overprint
                    let display_label = if label.len() > 24 {
                        format!("{}…", &label[..23])
                    } else {
                        label.to_string()
                    };
                    // Outlined (not filled) so a source reads as a secondary,
                    // link-like action, distinct from the solid follow-ups.
                    flat.push(json!({
                        "id": btn_id.clone(),
                        "component": "MaterialButton",
                        "label": format!("Open: {display_label}"),
                        "variant": "outlined",
                        "color": "primary",
                        "action": { "event": { "name": "open_doc", "context": {} } },
                    }));
                    source_btn_ids.push(btn_id);
                    // Cap source buttons in the card to at most 2 so they never crowd out action buttons
                    if source_btn_ids.len() >= 2 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    // Follow-up/report buttons → a small grid: MaterialRow has no wrap and no
    // gap, so cramming 4 buttons in one row squeezes them and long labels wrap
    // inside a fixed-height pill (visible "overprinting"). Chunk into rows of
    // two so each button gets room; the rows stack in the outer Column.
    // Action buttons are laid out first, followed by secondary source buttons
    // in their own row, keeping the layout clean and balanced.
    let mut row_idx = 0;
    for chunk in action_btn_ids.chunks(2) {
        let row_id = format!("btn-row-{row_idx}");
        row_idx += 1;
        flat.push(json!({
            "id": row_id.clone(),
            "component": "MaterialRow",
            "children": chunk,
            "justify": "start",
            "align": "stretch",
        }));
        root_children.push(row_id);
    }
    for chunk in source_btn_ids.chunks(2) {
        let row_id = format!("btn-row-{row_idx}");
        row_idx += 1;
        flat.push(json!({
            "id": row_id.clone(),
            "component": "MaterialRow",
            "children": chunk,
            "justify": "start",
            "align": "stretch",
        }));
        root_children.push(row_id);
    }

    if root_children.is_empty() {
        return (!passthrough.is_empty()).then_some(passthrough);
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

    let mut messages = passthrough;
    messages.extend([
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
    ]);
    Some(messages)
}

/// Upper bounds for producer-built surfaces: they share one process-wide
/// re-ask table with every user's cards, so one reply must not be able to
/// evict the rest.
const MAX_PASSTHROUGH_SURFACES: usize = 8;
const MAX_PASSTHROUGH_ACTIONS: usize = 32;
const MAX_ACTION_QUESTION_BYTES: usize = 1024;
const MAX_DISPLAY_NAME_CHARS: usize = 64;
const MAX_COMPONENT_ID_BYTES: usize = 128;

/// The v0.9 messages for the producer-built surfaces in `surface.a2ui`:
/// `[{components, data_model?, theme?, actions?}]`, at most
/// [`MAX_PASSTHROUGH_SURFACES`].
///
/// Each surface is emitted verbatim as createSurface + updateComponents (+
/// updateDataModel). `components` is the flat v0.9 component list and must
/// contain the `root` component; a surface without one is dropped. `actions`
/// maps a component id to the question its click re-asks (exact id match, see
/// [`question_for`]). `theme` may set `agentDisplayName` and `primaryColor`
/// (`#rrggbb`) only. The components themselves are the producer's
/// responsibility: Triton does not vet them, so passthrough is for upstreams
/// the operator trusts like its own code.
fn passthrough_surfaces(spec: Option<&Value>) -> Vec<Value> {
    let Some(surfaces) = spec.and_then(Value::as_array) else {
        return Vec::new();
    };
    if surfaces.len() > MAX_PASSTHROUGH_SURFACES {
        eprintln!(
            r#"{{"kind":"log","level":"warn","msg":"a2ui passthrough: surfaces beyond the cap dropped","surfaces":{},"cap":{MAX_PASSTHROUGH_SURFACES}}}"#,
            surfaces.len()
        );
    }
    surfaces
        .iter()
        .take(MAX_PASSTHROUGH_SURFACES)
        .flat_map(passthrough_messages)
        .collect()
}

fn passthrough_messages(surface: &Value) -> Vec<Value> {
    let Some(components) = surface.get("components").and_then(Value::as_array) else {
        return Vec::new();
    };
    if !components
        .iter()
        .any(|c| c.get("id").and_then(Value::as_str) == Some("root"))
    {
        eprintln!(
            r#"{{"kind":"log","level":"warn","msg":"a2ui passthrough: surface without a root component dropped"}}"#
        );
        return Vec::new();
    }
    let surface_id = format!("{PASSTHROUGH_PREFIX}{}", uuid::Uuid::new_v4());
    remember_actions(&surface_id, passthrough_actions(surface.get("actions")));
    let create = json!({
        "surfaceId": surface_id,
        "catalogId": BASIC_CATALOG,
        "theme": passthrough_theme(surface.get("theme")),
    });
    let mut messages = vec![
        json!({ "version": "v0.9", "createSurface": create }),
        json!({ "version": "v0.9", "updateComponents": { "surfaceId": surface_id, "components": components } }),
    ];
    if let Some(model) = surface.get("data_model").filter(|m| !m.is_null()) {
        messages.push(json!({
            "version": "v0.9",
            "updateDataModel": { "surfaceId": surface_id, "path": "/", "value": model },
        }));
    }
    messages
}

/// The producer's `actions`, filtered: string questions without control
/// characters (so a producer cannot mint Triton's internal open-doc sentinel)
/// and within the length cap; at most [`MAX_PASSTHROUGH_ACTIONS`].
fn passthrough_actions(spec: Option<&Value>) -> std::collections::HashMap<String, String> {
    let Some(actions) = spec.and_then(Value::as_object) else {
        return Default::default();
    };
    let valid: Vec<(String, String)> = actions
        .iter()
        .filter_map(|(component_id, question)| {
            let q = question.as_str()?;
            (component_id.len() <= MAX_COMPONENT_ID_BYTES
                && q.len() <= MAX_ACTION_QUESTION_BYTES
                && !q.chars().any(char::is_control))
            .then(|| (component_id.clone(), q.to_string()))
        })
        .collect();
    if valid.len() != actions.len() || valid.len() > MAX_PASSTHROUGH_ACTIONS {
        eprintln!(
            r#"{{"kind":"log","level":"warn","msg":"a2ui passthrough: actions dropped (invalid or beyond the cap)","given":{},"kept":{}}}"#,
            actions.len(),
            valid.len().min(MAX_PASSTHROUGH_ACTIONS)
        );
    }
    valid.into_iter().take(MAX_PASSTHROUGH_ACTIONS).collect()
}

/// Triton's surface theme with the producer's `agentDisplayName` and
/// `primaryColor` applied when they are well-formed; other keys are ignored.
fn passthrough_theme(spec: Option<&Value>) -> Value {
    let mut theme =
        json!({ "primaryColor": THEME_PRIMARY_COLOR, "agentDisplayName": "DataZoo Agent" });
    let Some(over) = spec.and_then(Value::as_object) else {
        return theme;
    };
    if let Some(name) = over
        .get("agentDisplayName")
        .and_then(Value::as_str)
        .map(str::trim)
        && !name.is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control)
    {
        theme["agentDisplayName"] = json!(name);
    }
    if let Some(color) = over.get("primaryColor").and_then(Value::as_str)
        && color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        theme["primaryColor"] = json!(color);
    }
    theme
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

/// Build the document as a **native Material** surface (a `MaterialCard` reply
/// in the chat stream) from a peacock `document` render's `structuredContent`:
/// a title, a `MaterialTable` of frontmatter facts, the markdown body as
/// `MaterialText`, and the event timeline. Uses ONLY the documented, GA
/// Material components — no `IFrameUrl`, no allowlist — so it renders in any
/// A2UI-GA tenant. `title` falls back to the instance id.
pub fn build_document_material(structured: &Value, title: &str) -> Vec<Value> {
    let inst = structured
        .get("instances")
        .and_then(Value::as_object)
        .and_then(|m| m.values().next());
    let heading = if title.is_empty() {
        inst.and_then(|i| i.get("id").and_then(Value::as_str))
            .unwrap_or("Document")
    } else {
        title
    };

    let mut flat: Vec<Value> = Vec::new();
    let mut children: Vec<String> = Vec::new();
    let mut n = 0usize;
    let mut add = |flat: &mut Vec<Value>, children: &mut Vec<String>, comp: Value| {
        n += 1;
        let cid = format!("d-{n}");
        let mut c = comp;
        c["id"] = json!(cid);
        flat.push(c);
        children.push(cid);
    };

    add(
        &mut flat,
        &mut children,
        json!({ "component": "MaterialText", "text": heading, "usageHint": "h3" }),
    );

    if let Some(inst) = inst {
        // Facts → a 2-column MaterialTable (rows are string-keyed objects).
        if let Some(facts) = inst.get("facts").and_then(Value::as_array) {
            let rows: Vec<Value> = facts
                .iter()
                .filter_map(|f| {
                    let k = f.get("key").and_then(Value::as_str)?;
                    let v = f.get("value").and_then(Value::as_str)?;
                    Some(json!({ "field": k, "value": v }))
                })
                .collect();
            if !rows.is_empty() {
                add(
                    &mut flat,
                    &mut children,
                    json!({
                        "component": "MaterialTable",
                        "columns": [
                            { "header": "Field", "field": "field" },
                            { "header": "Value", "field": "value" },
                        ],
                        "rows": rows,
                    }),
                );
            }
        }
        // Markdown body → MaterialText (GE renders plain text; still readable).
        if let Some(md) = inst.get("markdown").and_then(Value::as_str)
            && !md.trim().is_empty()
        {
            add(
                &mut flat,
                &mut children,
                json!({ "component": "MaterialText", "text": md, "usageHint": "body" }),
            );
        }
        // Timeline → a heading + one MaterialText line per event.
        if let Some(events) = inst.get("events").and_then(Value::as_array)
            && !events.is_empty()
        {
            add(
                &mut flat,
                &mut children,
                json!({ "component": "MaterialText", "text": "Activity", "usageHint": "subtitle2" }),
            );
            for e in events {
                let at = e.get("at").and_then(Value::as_str).unwrap_or("");
                let src = e.get("source").and_then(Value::as_str).unwrap_or("");
                let ttl = e.get("title").and_then(Value::as_str).unwrap_or("");
                let meta = [at, src]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                let line = if meta.is_empty() {
                    ttl.to_string()
                } else {
                    format!("{ttl} — {meta}")
                };
                add(
                    &mut flat,
                    &mut children,
                    json!({ "component": "MaterialText", "text": line, "usageHint": "caption" }),
                );
            }
        }
    }

    if children.len() == 1 {
        // Only the heading — nothing resolved; say so rather than a bare title.
        add(
            &mut flat,
            &mut children,
            json!({ "component": "MaterialText", "text": "This document is unavailable right now.", "usageHint": "body" }),
        );
    }

    flat.push(json!({ "id": "root-col", "component": "MaterialColumn", "children": children }));
    flat.push(json!({
        "id": "root", "component": "MaterialCard", "appearance": "outlined",
        "children": ["root-col"],
    }));

    let surface_id = format!("{SURFACE_PREFIX}{}", uuid::Uuid::new_v4());
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

    /// A producer that speaks v0.9 itself puts its surfaces in `surface.a2ui`:
    /// one createSurface + updateComponents (+ updateDataModel) per surface, in
    /// order, ahead of Triton's own card.
    #[test]
    fn a2ui_surfaces_pass_through_verbatim() {
        let result = json!({ "surface": {
            "components": [ { "kind": "text", "value": "Nine requirements, three close calls." } ],
            "a2ui": [
                {
                    "components": [
                        { "id": "root", "component": "MaterialCard", "children": ["tbl"] },
                        { "id": "tbl", "component": "MaterialTable",
                          "columns": [{ "header": "Requirement", "field": "req" }],
                          "rows": [{ "req": "Easy-Entry" }] }
                    ],
                    "data_model": { "picks": { "r04": "fold-flat mechanism" } },
                    "theme": { "agentDisplayName": "Coverage Matching", "primaryColor": "#004149" }
                },
                {
                    "components": [
                        { "id": "root", "component": "Canvas", "cardTitle": "Capability mapping",
                          "children": ["chart"] },
                        { "id": "chart", "component": "VegaChart", "spec": { "mark": "rule" } }
                    ]
                }
            ]
        }});
        let msgs = build_messages(&result).expect("messages");
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        let s1 = msgs[0]["createSurface"]["surfaceId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(msgs[0]["version"], "v0.9");
        assert_eq!(msgs[0]["createSurface"]["catalogId"], BASIC_CATALOG);
        assert_eq!(
            msgs[0]["createSurface"]["theme"]["agentDisplayName"],
            "Coverage Matching"
        );
        assert_eq!(msgs[0]["createSurface"]["theme"]["primaryColor"], "#004149");
        assert_eq!(msgs[1]["updateComponents"]["surfaceId"], s1);
        assert_eq!(
            msgs[1]["updateComponents"]["components"][1]["component"],
            "MaterialTable"
        );
        assert_eq!(msgs[2]["updateDataModel"]["surfaceId"], s1);
        assert_eq!(msgs[2]["updateDataModel"]["path"], "/");
        assert_eq!(
            msgs[2]["updateDataModel"]["value"]["picks"]["r04"],
            "fold-flat mechanism"
        );
        let s2 = msgs[3]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_ne!(s1, s2, "each surface has its own id");
        assert_eq!(
            msgs[3]["createSurface"]["theme"]["agentDisplayName"],
            "DataZoo Agent"
        );
        assert_eq!(
            msgs[4]["updateComponents"]["components"][0]["component"],
            "Canvas"
        );
    }

    /// Passthrough surfaces come first; Triton's own card (buttons, charts)
    /// follows as its own surface.
    #[test]
    fn a2ui_surfaces_precede_tritons_own_card() {
        let result = json!({ "surface": {
            "components": [ { "kind": "button", "label": "Again", "tool": "t", "args": { "question": "again" } } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        let msgs = build_messages(&result).expect("messages");
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[1]["updateComponents"]["components"][0]["component"],
            "MaterialText"
        );
        assert!(msgs[2]["createSurface"]["surfaceId"].is_string());
        assert_eq!(
            msgs[3]["updateComponents"]["components"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["id"],
            "root"
        );
    }

    /// `actions` re-ask by the EXACT component id: a producer names its own
    /// components, so `ok-7` or `btn-07` must not resolve to `btn-7`'s question.
    #[test]
    fn a2ui_actions_match_exact_component_ids() {
        let result = json!({ "surface": { "a2ui": [{
            "components": [
                { "id": "root", "component": "MaterialCard", "children": ["btn-7", "ok-7"] },
                { "id": "btn-7", "component": "MaterialButton", "label": "Confirm",
                  "action": { "event": { "name": "confirm", "context": {} } } },
                { "id": "ok-7", "component": "MaterialButton", "label": "Other",
                  "action": { "event": { "name": "other", "context": {} } } }
            ],
            "actions": { "btn-7": "Confirm the three close calls" }
        }]}});
        let msgs = build_messages(&result).expect("messages");
        let sid = msgs[0]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_eq!(
            question_for(sid, "btn-7").as_deref(),
            Some("Confirm the three close calls")
        );
        assert_eq!(question_for(sid, "ok-7"), None);
        assert_eq!(question_for(sid, "btn-07"), None);
        assert_eq!(question_for(sid, "btn-8"), None);
    }

    /// Two passthrough surfaces may both use `btn-1`; each click resolves to its
    /// own surface's question.
    #[test]
    fn a2ui_actions_are_per_surface() {
        let surface = |q: &str| {
            json!({
                "components": [
                    { "id": "root", "component": "MaterialCard", "children": ["btn-1"] },
                    { "id": "btn-1", "component": "MaterialButton", "label": "Go",
                      "action": { "event": { "name": "go", "context": {} } } }
                ],
                "actions": { "btn-1": q }
            })
        };
        let result = json!({ "surface": { "a2ui": [surface("first"), surface("second")] } });
        let msgs = build_messages(&result).expect("messages");
        let s1 = msgs[0]["createSurface"]["surfaceId"].as_str().unwrap();
        let s2 = msgs[2]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_eq!(question_for(s1, "btn-1").as_deref(), Some("first"));
        assert_eq!(question_for(s2, "btn-1").as_deref(), Some("second"));
    }

    /// A producer cannot mint Triton's internal open-doc sentinel (or any
    /// control character) through an action question, nor an oversized one.
    #[test]
    fn a2ui_actions_with_control_characters_or_oversized_are_dropped() {
        let forged = format!("{DOC_OPEN_SENTINEL}x\u{1}\u{1}\u{1}http://evil.example/");
        let result = json!({ "surface": { "a2ui": [{
            "components": [ { "id": "root", "component": "MaterialCard", "children": [] } ],
            "actions": { "btn-1": forged, "btn-2": "x".repeat(MAX_ACTION_QUESTION_BYTES + 1), "btn-3": "fine" }
        }]}});
        let msgs = build_messages(&result).expect("messages");
        let sid = msgs[0]["createSurface"]["surfaceId"].as_str().unwrap();
        assert_eq!(question_for(sid, "btn-1"), None);
        assert_eq!(question_for(sid, "btn-2"), None);
        assert_eq!(question_for(sid, "btn-3").as_deref(), Some("fine"));
    }

    /// Only `agentDisplayName` and `primaryColor` are taken from the producer,
    /// and only when well-formed.
    #[test]
    fn a2ui_theme_is_allowlisted() {
        let result = json!({ "surface": { "a2ui": [{
            "components": [ { "id": "root", "component": "MaterialCard", "children": [] } ],
            "theme": { "agentDisplayName": "x".repeat(MAX_DISPLAY_NAME_CHARS + 1),
                       "primaryColor": "red", "logoUrl": "https://evil.example/l.png" }
        }]}});
        let msgs = build_messages(&result).expect("messages");
        let theme = &msgs[0]["createSurface"]["theme"];
        assert_eq!(theme["agentDisplayName"], "DataZoo Agent");
        assert_eq!(theme["primaryColor"], THEME_PRIMARY_COLOR);
        assert!(theme.get("logoUrl").is_none());
    }

    /// At most MAX_PASSTHROUGH_SURFACES surfaces go out per result.
    #[test]
    fn a2ui_surfaces_are_capped() {
        let one =
            json!({ "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] });
        let result = json!({ "surface": { "a2ui": vec![one; MAX_PASSTHROUGH_SURFACES + 3] } });
        let msgs = build_messages(&result).expect("messages");
        let creates = msgs
            .iter()
            .filter(|m| m.get("createSurface").is_some())
            .count();
        assert_eq!(creates, MAX_PASSTHROUGH_SURFACES);
    }

    /// The open-doc round trip only ever opens https documents.
    #[test]
    fn open_doc_refuses_non_https_urls() {
        let forged = format!("{DOC_OPEN_SENTINEL}label\u{1}\u{1}\u{1}http://evil.example/");
        assert!(open_doc(&forged).is_none());
        let fine = format!("{DOC_OPEN_SENTINEL}label\u{1}\u{1}\u{1}https://docs.example/x");
        assert!(open_doc(&fine).is_some());
    }

    /// Invalid actions do not count against the cap; ids over the length cap
    /// are dropped; an unknown passthrough surface resolves to nothing.
    #[test]
    fn a2ui_action_cap_counts_valid_entries_only() {
        let mut actions = serde_json::Map::new();
        for n in 0..10 {
            actions.insert(format!("bad-{n}"), json!(42));
        }
        for n in 0..MAX_PASSTHROUGH_ACTIONS + 5 {
            actions.insert(format!("ok-{n}"), json!(format!("q{n}")));
        }
        actions.insert("x".repeat(MAX_COMPONENT_ID_BYTES + 1), json!("long id"));
        let kept = passthrough_actions(Some(&Value::Object(actions)));
        assert_eq!(kept.len(), MAX_PASSTHROUGH_ACTIONS);
        assert!(kept.values().all(|q| q.starts_with('q')));
        assert_eq!(question_for("triton-a2ui-unknown", "ok-1"), None);
    }

    /// Without A2UI negotiated only Triton's own card goes out.
    #[test]
    fn negotiated_builder_drops_passthrough_without_a2ui() {
        let result = json!({ "surface": {
            "components": [ { "kind": "button", "label": "Again", "tool": "t", "args": { "question": "again" } } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        assert_eq!(build_messages_negotiated(&result, true).unwrap().len(), 4);
        let plain = build_messages_negotiated(&result, false).unwrap();
        assert_eq!(plain.len(), 2);
        assert!(
            plain[0]["createSurface"]["surfaceId"]
                .as_str()
                .unwrap()
                .starts_with(SURFACE_PREFIX)
        );
    }

    /// A surface without a `root` component cannot render; it is dropped
    /// rather than sent to a client that would show an empty card.
    #[test]
    fn a2ui_surface_without_root_is_dropped() {
        let result = json!({ "surface": { "a2ui": [{ "components": [
            { "id": "card", "component": "MaterialCard", "children": [] }
        ]}]}});
        assert!(build_messages(&result).is_none());
    }

    /// The typed vocabulary every other channel uses ignores `surface.a2ui`, so
    /// a producer's passthrough never breaks a chat channel or MCP.
    #[test]
    fn typed_consumers_ignore_the_a2ui_sibling() {
        let result = json!({ "surface": {
            "components": [ { "kind": "text", "value": "still here" } ],
            "a2ui": [ { "components": [ { "id": "root", "component": "MaterialText", "text": "x" } ] } ]
        }});
        let typed = crate::a2ui::extract_surface(&result).expect("typed surface");
        assert_eq!(typed.components.len(), 1);
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
        // layer parses back out: label, skill, id (for the native render) + url.
        let stored = format!(
            "{DOC_OPEN_SENTINEL}account \u{b7} beverages\u{1}account\u{1}beverages\u{1}https://x/docs/tok"
        );
        let doc = open_doc(&stored).expect("parses");
        assert_eq!(doc.label, "account \u{b7} beverages");
        assert_eq!(doc.skill, "account");
        assert_eq!(doc.id, "beverages");
        assert_eq!(doc.url, "https://x/docs/tok");
        // An ordinary re-ask question is not an open-doc sentinel.
        assert!(open_doc("What does Initech buy?").is_none());
    }

    #[test]
    fn build_document_material_uses_native_widgets() {
        // The allowlist-free GE render: MaterialCard/Table/Text, no IFrameUrl.
        let structured = json!({
            "document": { "skill": "account", "id": "beverages" },
            "instances": { "self": {
                "id": "beverages", "skill": "account",
                "facts": [ { "key": "name", "value": "Beverages GmbH" } ],
                "markdown": "Renewal at risk.",
                "events": [ { "title": "Flagged", "at": "2026-09-01", "source": "analyst" } ],
            } },
        });
        let msgs = build_document_material(&structured, "account \u{b7} beverages");
        let comps = msgs[1]["updateComponents"]["components"]
            .as_array()
            .unwrap();
        assert_eq!(find(comps, "root")["component"], "MaterialCard");
        assert!(comps.iter().any(|c| c["component"] == "MaterialTable"
            && c["rows"][0]["field"] == "name"
            && c["rows"][0]["value"] == "Beverages GmbH"));
        assert!(
            comps
                .iter()
                .any(|c| c["component"] == "MaterialText" && c["text"] == "Renewal at risk.")
        );
        // No IFrameUrl / Canvas — this is the documented-widget path.
        assert!(!comps.iter().any(|c| c["component"] == "IFrameUrl"));
        assert!(!comps.iter().any(|c| c["component"] == "Canvas"));
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
