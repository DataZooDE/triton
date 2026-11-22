//! The upstream's own result `_meta` on an MCP `tools/call` result (#375):
//! an allow-list.
//!
//! An upstream's `_meta` keys reach an MCP client only when this list names
//! them, each with a shape check and an audience:
//!
//! | key | shape | audience |
//! |---|---|---|
//! | `ui` | object | every client: an upstream MCP App's resource link (#143); the host decides whether to render it |
//! | `tool_trace` | array | every client: an opt-in debug sidecar (decided 2026-11-18) |
//! | `chart` | object | card hosts: the neutral chart summary |
//! | `png_base64` | string | card hosts: the chart image |
//! | `svgs` | array | card hosts: the painted charts |
//!
//! A card host is a client that renders the answer card (it declared MCP
//! Apps and the widget is on); like the card itself (#379), chart data goes
//! to such clients only. The operator adds keys per tool, or gives a listed
//! key another audience, in `adapter.yaml` (`tools.<name>.meta`, any shape).
//! Every other key is dropped, so the list fails closed. A Triton-owned or
//! MCP-reserved key can never be listed
//! ([`triton_manifest::upstream_meta_key_reserved`]). The passed keys share
//! [`BUDGET`].

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};
pub use triton_manifest::MetaAudience;
use triton_manifest::upstream_meta_key_reserved;

/// Combined serialized size of the passed keys. A key that would exceed it
/// is dropped (and logged); the keys before it still pass.
pub const BUDGET: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum Shape {
    Object,
    Array,
    String,
    /// An operator-listed key: any JSON value.
    Any,
}

impl Shape {
    fn fits(self, v: &Value) -> bool {
        match self {
            Shape::Object => v.is_object(),
            Shape::Array => v.is_array(),
            Shape::String => v.is_string(),
            Shape::Any => true,
        }
    }
}

/// The built-in list, in the order keys are taken from the budget.
const BUILT_IN: &[(&str, Shape, MetaAudience)] = &[
    ("ui", Shape::Object, MetaAudience::All),
    ("tool_trace", Shape::Array, MetaAudience::All),
    ("chart", Shape::Object, MetaAudience::CardHost),
    ("png_base64", Shape::String, MetaAudience::CardHost),
    ("svgs", Shape::Array, MetaAudience::CardHost),
];

/// The allow-list: the built-in keys and the operator's per-tool entries.
#[derive(Debug, Clone, Default)]
pub struct MetaPolicy {
    per_tool: HashMap<String, BTreeMap<String, MetaAudience>>,
}

impl MetaPolicy {
    /// The policy with the operator's per-tool entries
    /// (`Manifest::meta_passthrough`). A reserved key is ignored (the
    /// manifest validation refuses it before this).
    pub fn new(per_tool: BTreeMap<String, BTreeMap<String, MetaAudience>>) -> Self {
        let per_tool = per_tool
            .into_iter()
            .map(|(tool, keys)| {
                let keys = keys
                    .into_iter()
                    .filter(|(k, _)| !upstream_meta_key_reserved(k))
                    .collect();
                (tool, keys)
            })
            .collect();
        Self { per_tool }
    }

    /// The upstream `_meta` keys of `tool`'s `result` that a client gets
    /// (`card_host`: it renders the card), in list order, within
    /// [`BUDGET`]. A non-object `_meta` passes nothing.
    pub fn select(&self, tool: &str, result: &Value, card_host: bool) -> Map<String, Value> {
        let mut out = Map::new();
        let Some(meta) = result.get("_meta").and_then(Value::as_object) else {
            return out;
        };
        let extra = self.per_tool.get(tool);
        let configured = extra.into_iter().flatten().filter_map(|(k, a)| {
            (!BUILT_IN.iter().any(|(b, _, _)| b == k)).then_some((k.as_str(), Shape::Any, *a))
        });
        let rules = BUILT_IN
            .iter()
            .map(|&(k, shape, audience)| {
                let audience = extra.and_then(|e| e.get(k)).copied().unwrap_or(audience);
                (k, shape, audience)
            })
            .chain(configured);
        let mut used = 0usize;
        for (key, shape, audience) in rules {
            let admitted = match audience {
                MetaAudience::All => true,
                MetaAudience::CardHost => card_host,
                MetaAudience::Off => false,
            };
            let Some(v) = meta.get(key).filter(|_| admitted) else {
                continue;
            };
            if !shape.fits(v) {
                tracing::debug!(key, "mcp: upstream _meta key has the wrong shape; dropped");
                continue;
            }
            let size = key.len() + serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
            if used.saturating_add(size) > BUDGET {
                tracing::warn!(
                    key,
                    bytes = size,
                    "mcp: upstream _meta key over the size budget; dropped"
                );
                continue;
            }
            used += size;
            out.insert(key.to_string(), v.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(tool: &str, entries: &[(&str, MetaAudience)]) -> MetaPolicy {
        let keys = entries.iter().map(|(k, a)| (k.to_string(), *a)).collect();
        MetaPolicy::new(BTreeMap::from([(tool.to_string(), keys)]))
    }

    #[test]
    fn listed_keys_pass_by_audience_and_shape_everything_else_is_dropped() {
        let r = json!({ "_meta": {
            "ui": { "resourceUri": "ui://x/y" }, "tool_trace": [1],
            "chart": {}, "png_base64": "AA", "svgs": [], "vega_specs": [],
            "other": 1, "trace_id": "forged", "triton/a2ui": [] } });
        let host = MetaPolicy::default().select("t", &r, true);
        let mut keys: Vec<&str> = host.keys().map(String::as_str).collect();
        keys.sort_unstable();
        // `vega_specs` is no longer listed (#382).
        assert_eq!(keys, ["chart", "png_base64", "svgs", "tool_trace", "ui"]);
        let plain = MetaPolicy::default().select("t", &r, false);
        let mut keys: Vec<&str> = plain.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["tool_trace", "ui"]);
        // Wrong shapes are dropped.
        let bad = json!({ "_meta": { "ui": "x", "tool_trace": {}, "png_base64": 1 } });
        assert!(MetaPolicy::default().select("t", &bad, true).is_empty());
        assert!(
            MetaPolicy::default()
                .select("t", &json!({ "_meta": [1] }), true)
                .is_empty()
        );
    }

    #[test]
    fn the_operator_adds_widens_and_switches_off_per_tool() {
        let p = policy(
            "t",
            &[
                ("custom", MetaAudience::All),
                ("png_base64", MetaAudience::All),
                ("chart", MetaAudience::Off),
                ("trace_id", MetaAudience::All),
            ],
        );
        let r =
            json!({ "_meta": { "custom": 7, "png_base64": "AA", "chart": {}, "trace_id": "x" } });
        let plain = p.select("t", &r, false);
        assert_eq!(plain.get("custom"), Some(&json!(7)));
        assert_eq!(plain.get("png_base64"), Some(&json!("AA")));
        assert!(plain.get("chart").is_none());
        assert!(plain.get("trace_id").is_none(), "reserved keys stay out");
        // Another tool keeps the built-in list.
        assert!(p.select("other", &r, false).is_empty());
    }

    #[test]
    fn keys_beyond_the_budget_are_dropped_one_by_one() {
        let big = "x".repeat(BUDGET);
        let r = json!({ "_meta": { "chart": { "k": 1 }, "png_base64": big, "svgs": ["<svg/>"] } });
        let m = MetaPolicy::default().select("t", &r, true);
        assert!(m.contains_key("chart"));
        assert!(!m.contains_key("png_base64"));
        assert!(m.contains_key("svgs"));
    }
}
