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

/// Subset of the spec's component vocabulary needed for the
/// integration tests. Adding richer components (selection, form,
/// dashboard) means appending variants here AND extending both
/// builders to handle them.
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
    },
    Narration {
        text: String,
    },
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
