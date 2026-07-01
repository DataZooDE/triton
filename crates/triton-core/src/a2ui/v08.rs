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

use super::{Component, FormFieldKind, Surface};

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
        Component::Selection {
            prompt,
            options,
            tool,
            args_key,
        } => json!({
            "Selection": {
                "prompt": prompt,
                "options": options.iter().map(|o| json!({
                    "label": o.label,
                    "value": o.value,
                })).collect::<Vec<_>>(),
                "action": { "tool": tool, "args_key": args_key }
            }
        }),
        Component::Form {
            title,
            fields,
            submit_label,
            tool,
        } => json!({
            "Form": {
                "title": title,
                "fields": fields.iter().map(|f| json!({
                    "name": f.name,
                    "label": f.label,
                    "kind": form_kind_str(f.kind),
                    "required": f.required,
                })).collect::<Vec<_>>(),
                "submit_label": submit_label,
                "action": { "tool": tool }
            }
        }),
        Component::Dashboard { title, tiles } => json!({
            "Dashboard": {
                "title": title,
                "tiles": tiles.iter().map(|t| {
                    let mut o = json!({ "label": t.label, "value": t.value });
                    if let Some(trend) = &t.trend {
                        o["trend"] = json!(trend);
                    }
                    o
                }).collect::<Vec<_>>(),
            }
        }),
        Component::Report { report_id, args } => json!({
            "Report": { "report_id": report_id, "args": args }
        }),
    };
    json!({ "Component": inner })
}

fn form_kind_str(k: FormFieldKind) -> &'static str {
    match k {
        FormFieldKind::String => "string",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
    }
}
