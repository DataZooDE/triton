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
}
