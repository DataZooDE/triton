//! A2UI v0.9 envelope builder. **No shared base** with v0.8 per
//! ADR-4. v0.9 flattens each stream entry — no `Component`
//! wrapper, lowercase `type` field, action data inlined:
//!
//! ```json
//! {
//!   "version": "0.9",
//!   "stream": [
//!     { "type": "text", "text": "Hello" },
//!     { "type": "button", "label": "Click", "action": {...} }
//!   ]
//! }
//! ```

use serde_json::{Value, json};

use super::{Component, Surface};

pub fn build(surface: &Surface) -> Value {
    let stream: Vec<Value> = surface.components.iter().map(component_to_json).collect();
    json!({
        "version": "0.9",
        "stream": stream
    })
}

fn component_to_json(c: &Component) -> Value {
    match c {
        Component::Text { value } => json!({ "type": "text", "text": value }),
        Component::Narration { text } => json!({ "type": "narration", "text": text }),
        Component::Button { label, tool, args } => json!({
            "type": "button",
            "label": label,
            "action": { "tool": tool, "args": args }
        }),
    }
}
