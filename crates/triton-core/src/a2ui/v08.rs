//! A2UI v0.8 envelope builder. **No shared base** with v0.9 per
//! ADR-4 — this file owns the full v0.8 shape and must remain
//! self-contained as the schema evolves.
//!
//! v0.8 wraps every stream entry in a PascalCase `Component` field
//! carrying a typed inner object:
//!
//! ```json
//! {
//!   "version": "0.8",
//!   "stream": [
//!     { "Component": { "Text": { "text": "Hello" } } },
//!     { "Component": { "Button": { "label": "Click", "action": {...} } } }
//!   ]
//! }
//! ```

use serde_json::{Value, json};

use super::{Component, Surface};

pub fn build(surface: &Surface) -> Value {
    let stream: Vec<Value> = surface.components.iter().map(component_to_json).collect();
    json!({
        "version": "0.8",
        "stream": stream
    })
}

fn component_to_json(c: &Component) -> Value {
    let inner = match c {
        Component::Text { value } => json!({ "Text": { "text": value } }),
        Component::Narration { text } => json!({ "Narration": { "text": text } }),
        Component::Button { label, tool, args } => json!({
            "Button": {
                "label": label,
                "action": { "tool": tool, "args": args }
            }
        }),
    };
    json!({ "Component": inner })
}
