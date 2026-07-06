//! A2UI envelope builders (ADR-4, FR-A-3..A-5).
//!
//! Two **independent** modules, one per version. Python experiment
//! finding (realizations §1): schema evolution stays compartmentalised
//! when each version owns its own builder file. The dispatcher and
//! tools never branch on version — only the adapter-edge wrap layer
//! does.
//!
//! Tools that emit a UI surface return JSON of shape
//! `{ "surface": { "components": [ ... ] } }`. The adapter, when the
//! caller has negotiated A2UI, deserialises the `surface` value
//! into [`Surface`] and hands it to the version's `build` function.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod v08;
pub mod v09;

/// Canonical, version-agnostic surface a tool returns. Designed so
/// adding a new version means adding one more builder file — never
/// changing this struct in a way that breaks existing builders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub components: Vec<Component>,
}

/// Component vocabulary. Both v0.8 and v0.9 builders must handle
/// every variant; appending a new variant means touching both
/// `v08::component_to_json` and `v09::component_to_json` (the
/// compiler enforces this via the non-exhaustive `match`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Component {
    Text {
        value: String,
    },
    Button {
        label: String,
        tool: String,
        args: Value,
        /// Optional MCP-App the button references (`ui://<authority>/…`,
        /// params in the query) — the report-as-surface pattern. Hosts
        /// that can embed resources may auto-open it; others ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource: Option<String>,
    },
    Narration {
        text: String,
    },
    /// Pick one option from a list and re-invoke a tool with the
    /// chosen value bound to `args_key`. This is the "buttons with
    /// state" rung of the L6′ degradation ladder (FR-D-3): cheaper
    /// surfaces (Telegram inline keyboards) flatten to numbered
    /// prompts, richer surfaces (web) render a SegmentedButton.
    Selection {
        prompt: String,
        options: Vec<SelectionOption>,
        tool: String,
        args_key: String,
    },
    /// Multi-field structured input that submits as one tool call.
    /// Used by surfaces that can host real forms (web, Adaptive
    /// Cards); flattens to a sequence of numbered prompts on
    /// text-only surfaces.
    Form {
        title: String,
        fields: Vec<FormField>,
        submit_label: String,
        tool: String,
    },
    /// Read-only grid of summary tiles. The rasteriser turns this
    /// into a PNG for surfaces that can't render tables (Telegram);
    /// web renders it as native Material cards.
    Dashboard {
        title: String,
        tiles: Vec<DashboardTile>,
    },
    /// A rendered report to embed inline: the adapter dispatches
    /// `render_report(report_id, args)` to the report upstream and
    /// shows the returned chart image in the same reply — so an agent
    /// can surface a rich chart without the user clicking a button.
    /// Surfaces that can't render images degrade to a caption/link.
    Report {
        report_id: String,
        #[serde(default)]
        args: Value,
    },
    /// Click-to-open references — "Sources" — to the documents a reply
    /// touched (created/updated records). Each item names an MCP-App
    /// resource; hosts render a chip row and open an item's resource
    /// ONLY on click. The items' resources deliberately sit one level
    /// down so an auto-open lift (first resource-bearing component)
    /// never fires on sources. Text-only surfaces degrade to a list of
    /// labels.
    Sources {
        items: Vec<SourceItem>,
    },
}

/// One clickable reference in a [`Component::Sources`] row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceItem {
    pub label: String,
    /// The MCP-App resource the click opens (`ui://<authority>/…`).
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: FormFieldKind,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFieldKind {
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardTile {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<String>,
}

/// A2UI versions Triton speaks (mirrors [`crate::A2uiVersion`] but
/// owned by the builder module). PR 10 ships v0.8 + v0.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderVersion {
    V08,
    V09,
}

/// Parse a tool's raw JSON result as a [`Surface`]. Fails when the
/// tool advertises `returns_a2ui = true` but emits a shape that
/// doesn't deserialise — caller is expected to surface the error
/// (typically as a `TritonError::Tool`) so the bug shows up at the
/// API boundary instead of being silently downgraded to raw JSON.
pub fn extract_surface(result: &Value) -> Result<Surface, String> {
    let surface = result
        .get("surface")
        .ok_or_else(|| "tool emitted no `surface` field".to_string())?;
    serde_json::from_value(surface.clone()).map_err(|e| format!("invalid A2UI surface: {e}"))
}

/// Build a version-specific envelope around a [`Surface`].
pub fn build_envelope(surface: &Surface, version: BuilderVersion) -> Value {
    match version {
        BuilderVersion::V08 => v08::build(surface),
        BuilderVersion::V09 => v09::build(surface),
    }
}

impl From<crate::A2uiVersion> for BuilderVersion {
    fn from(v: crate::A2uiVersion) -> Self {
        match v {
            crate::A2uiVersion::V08 => BuilderVersion::V08,
            crate::A2uiVersion::V09 => BuilderVersion::V09,
        }
    }
}

/// Reverse of the v0.9 [`build_envelope`]: recover the canonical
/// [`Surface`] from an already-negotiated envelope. The chat-channel
/// preview endpoint (`POST /v1/surface/render`) uses this so it can map a
/// turn the caller is *already showing* — the Explorer POSTs the bubble's
/// raw envelope — without re-invoking the tool (which, for an LLM agent,
/// would run a whole new turn and could yield a different surface).
///
/// It searches (bounded) for the `stream` array, tolerating the
/// per-protocol wrapping (`{result: …}` for REST/A2A, `{structuredContent:
/// …}` for MCP). Returns `None` when there is no v0.9 stream — a raw
/// `{surface}` (handled directly by [`extract_surface`]) or a v0.8 envelope
/// both yield `None`, and the caller falls back accordingly.
pub fn envelope_to_surface(value: &Value) -> Option<Surface> {
    let stream = find_stream(value, 0)?;
    let components = stream
        .iter()
        .map(v09_node_to_component)
        .collect::<Option<Vec<_>>>()?;
    Some(Surface { components })
}

/// Bounded search for a v0.9 `stream` array, descending through whatever
/// per-protocol envelope wraps it.
fn find_stream(value: &Value, depth: usize) -> Option<&Vec<Value>> {
    if depth > 6 {
        return None;
    }
    if let Some(stream) = value.get("stream").and_then(Value::as_array) {
        return Some(stream);
    }
    if let Value::Object(map) = value {
        for v in map.values() {
            if let Some(stream) = find_stream(v, depth + 1) {
                return Some(stream);
            }
        }
    }
    None
}

/// Turn one v0.9 stream node back into its canonical [`Component`] JSON
/// (internally `kind`-tagged) and deserialise it. This is the exact
/// inverse of `v09::component_to_json`: `type` → `kind`, Text's `text` →
/// `value`, and the button `action` object is unwrapped back to flat
/// `tool` / `args`. Every other field name already matches the struct, so
/// it rides through unchanged.
fn v09_node_to_component(node: &Value) -> Option<Component> {
    let ty = node.get("type")?.as_str()?;
    let mut obj = serde_json::Map::new();
    obj.insert("kind".to_string(), Value::String(ty.to_string()));
    let empty = || Value::Object(serde_json::Map::new());
    match ty {
        "text" => {
            obj.insert("value".into(), node.get("text")?.clone());
        }
        "narration" => {
            obj.insert("text".into(), node.get("text")?.clone());
        }
        "button" => {
            obj.insert("label".into(), node.get("label")?.clone());
            let action = node.get("action")?;
            obj.insert("tool".into(), action.get("tool")?.clone());
            obj.insert(
                "args".into(),
                action.get("args").cloned().unwrap_or_else(empty),
            );
            if let Some(r) = node.get("resource") {
                obj.insert("resource".into(), r.clone());
            }
        }
        "selection" => {
            for k in ["prompt", "options", "tool", "args_key"] {
                obj.insert(k.to_string(), node.get(k)?.clone());
            }
        }
        "form" => {
            for k in ["title", "fields", "submit_label", "tool"] {
                obj.insert(k.to_string(), node.get(k)?.clone());
            }
        }
        "dashboard" => {
            for k in ["title", "tiles"] {
                obj.insert(k.to_string(), node.get(k)?.clone());
            }
        }
        "report" => {
            obj.insert("report_id".into(), node.get("report_id")?.clone());
            obj.insert(
                "args".into(),
                node.get("args").cloned().unwrap_or_else(empty),
            );
        }
        "sources" => {
            obj.insert("items".into(), node.get("items")?.clone());
        }
        _ => return None,
    }
    serde_json::from_value(Value::Object(obj)).ok()
}

#[cfg(test)]
mod reverse_tests {
    use super::*;
    use serde_json::json;

    fn sample_surface() -> Surface {
        Surface {
            components: vec![
                Component::Text { value: "hi".into() },
                Component::Narration {
                    text: "note".into(),
                },
                Component::Button {
                    label: "Open".into(),
                    tool: "render_report".into(),
                    args: json!({ "id": "x" }),
                    resource: Some("ui://peacock/x".into()),
                },
                Component::Selection {
                    prompt: "pick".into(),
                    options: vec![SelectionOption {
                        label: "A".into(),
                        value: "a".into(),
                    }],
                    tool: "t".into(),
                    args_key: "k".into(),
                },
                Component::Form {
                    title: "F".into(),
                    fields: vec![FormField {
                        name: "n".into(),
                        label: "L".into(),
                        kind: FormFieldKind::Integer,
                        required: true,
                    }],
                    submit_label: "Go".into(),
                    tool: "t".into(),
                },
                Component::Dashboard {
                    title: "D".into(),
                    tiles: vec![DashboardTile {
                        label: "x".into(),
                        value: "1".into(),
                        trend: Some("up".into()),
                    }],
                },
                Component::Report {
                    report_id: "r".into(),
                    args: json!({ "a": 1 }),
                },
                Component::Sources {
                    items: vec![SourceItem {
                        label: "s".into(),
                        resource: "ui://peacock/d".into(),
                    }],
                },
            ],
        }
    }

    /// The whole point: `build` then reverse is the identity, across every
    /// component variant — so the preview endpoint can trust a round-tripped
    /// surface to render byte-for-byte what the live courier would.
    #[test]
    fn v09_envelope_round_trips_to_surface() {
        let s = sample_surface();
        let env = v09::build(&s);
        let back = envelope_to_surface(&env).expect("v0.9 envelope is reversible");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&s).unwrap(),
        );
    }

    /// REST/A2A nest the envelope under `result`; the finder descends into it.
    #[test]
    fn tolerates_a_result_wrapper() {
        let s = sample_surface();
        let wrapped = json!({ "result": v09::build(&s) });
        assert!(envelope_to_surface(&wrapped).is_some());
    }

    /// A raw `{surface}` has no stream → `None`, so the endpoint falls back to
    /// `extract_surface` instead of double-handling it.
    #[test]
    fn raw_surface_is_not_a_v09_envelope() {
        let raw = json!({ "surface": { "components": [] } });
        assert!(envelope_to_surface(&raw).is_none());
    }
}

#[cfg(test)]
mod resource_roundtrip_tests {
    use super::*;
    use serde_json::json;

    /// A surface BUTTON may reference an MCP-App (`resource: ui://…`) — the
    /// agent's report-as-surface pattern. The negotiated A2UI path must
    /// PRESERVE it through extract → build (it was silently dropped before),
    /// or hosts can never auto-open the referenced report.
    #[test]
    fn button_resource_survives_the_negotiated_round_trip() {
        let raw = json!({
            "surface": { "components": [{
                "kind": "button",
                "label": "Open report: nba-report",
                "tool": "render_report",
                "args": { "report_id": "nba-report", "params": { "account": "beverages-gmbh" } },
                "resource": "ui://peacock/nba-report?account=beverages-gmbh",
            }]}
        });
        let surface = extract_surface(&raw).expect("valid surface");
        for version in [BuilderVersion::V08, BuilderVersion::V09] {
            let envelope = build_envelope(&surface, version);
            let s = envelope.to_string();
            assert!(
                s.contains("ui://peacock/nba-report?account=beverages-gmbh"),
                "{version:?} must keep the button's resource: {s}"
            );
        }
        // A button WITHOUT a resource stays byte-shaped as before (no null key).
        let plain = extract_surface(&json!({
            "surface": { "components": [{
                "kind": "button", "label": "Ask again", "tool": "assistant", "args": {}
            }]}
        }))
        .expect("valid surface");
        let envelope = build_envelope(&plain, BuilderVersion::V09);
        assert!(!envelope.to_string().contains("resource"));
    }

    /// The SOURCES component: click-to-open references to the documents the
    /// agent touched this turn. Items carry their `resource` one level down
    /// (`items[].resource`) — NEVER as a top-level component key — so a
    /// host's "auto-open the first resource-bearing component" lift skips
    /// sources by construction and only an explicit click opens one.
    #[test]
    fn sources_round_trips_and_never_leaks_a_top_level_resource() {
        let raw = json!({
            "surface": { "components": [{
                "kind": "sources",
                "items": [
                    { "label": "account · initech-corp",
                      "resource": "ui://peacock/document?skill=account&id=initech-corp" },
                    { "label": "email · email-demo-1",
                      "resource": "ui://peacock/document?skill=email&id=email-demo-1" },
                ],
            }]}
        });
        let surface = extract_surface(&raw).expect("valid surface");
        for version in [BuilderVersion::V08, BuilderVersion::V09] {
            let envelope = build_envelope(&surface, version);
            let stream = envelope["stream"].as_array().expect("stream");
            assert_eq!(stream.len(), 1, "{version:?}: {envelope}");
            let node = &stream[0];
            // The wire node itself must NOT carry a `resource` key…
            let obj = match version {
                BuilderVersion::V09 => node.clone(),
                BuilderVersion::V08 => node["Component"]["Sources"].clone(),
            };
            assert!(
                obj.get("resource").is_none(),
                "{version:?} leaks a top-level resource: {node}"
            );
            // …while both items ride under `items` with label + resource.
            let items = obj["items"].as_array().expect("items");
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["label"], "account · initech-corp");
            assert_eq!(
                items[1]["resource"],
                "ui://peacock/document?skill=email&id=email-demo-1"
            );
        }
    }
}
