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

use super::{Component, FormFieldKind, Surface};

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
        Component::Button {
            label,
            tool,
            args,
            resource,
        } => {
            let mut b = json!({
                "type": "button",
                "label": label,
                "action": { "tool": tool, "args": args }
            });
            if let Some(r) = resource {
                b["resource"] = json!(r);
            }
            b
        }
        Component::Selection {
            prompt,
            options,
            tool,
            args_key,
        } => json!({
            "type": "selection",
            "prompt": prompt,
            "options": options.iter().map(|o| json!({
                "label": o.label,
                "value": o.value,
            })).collect::<Vec<_>>(),
            "tool": tool,
            "args_key": args_key,
        }),
        Component::Form {
            title,
            fields,
            submit_label,
            tool,
        } => json!({
            "type": "form",
            "title": title,
            "fields": fields.iter().map(|f| json!({
                "name": f.name,
                "label": f.label,
                "kind": form_kind_str(f.kind),
                "required": f.required,
            })).collect::<Vec<_>>(),
            "submit_label": submit_label,
            "tool": tool,
        }),
        Component::Dashboard { title, tiles } => json!({
            "type": "dashboard",
            "title": title,
            "tiles": tiles.iter().map(|t| {
                let mut o = json!({ "label": t.label, "value": t.value });
                if let Some(trend) = &t.trend {
                    o["trend"] = json!(trend);
                }
                o
            }).collect::<Vec<_>>(),
        }),
        Component::Report { report_id, args } => json!({
            "type": "report",
            "report_id": report_id,
            "args": args,
        }),
        Component::Sources { items } => json!({
            "type": "sources",
            "items": items.iter().map(|i| json!({
                "label": i.label,
                "resource": i.resource,
            })).collect::<Vec<_>>(),
        }),
    }
}

fn form_kind_str(k: FormFieldKind) -> &'static str {
    match k {
        FormFieldKind::String => "string",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
    }
}
