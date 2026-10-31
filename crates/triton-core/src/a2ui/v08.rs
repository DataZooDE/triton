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

use super::{Component, FormField, FormFieldKind, Surface};

pub fn build(surface: &Surface) -> Value {
    let stream: Vec<Value> = surface.components.iter().map(component_to_json).collect();
    json!({
        "version": "0.8",
        "stream": stream
    })
}

fn component_to_json(c: &Component) -> Value {
    let inner = match c {
        Component::Text { value, pills } => {
            let mut t = json!({ "text": value });
            if !pills.is_empty() {
                t["pills"] = json!(pills);
            }
            json!({ "Text": t })
        }
        Component::Narration { text } => json!({ "Narration": { "text": text } }),
        Component::Button {
            label,
            tool,
            args,
            action,
            resource,
            primary,
        } => {
            let mut b = json!({
                "label": label,
                "action": super::button_action(tool, args, action.as_ref())
            });
            if let Some(r) = resource {
                b["resource"] = json!(r);
            }
            if *primary {
                b["primary"] = json!(true);
            }
            json!({ "Button": b })
        }
        Component::Selection {
            prompt,
            options,
            tool,
            args_key,
            ..
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
            ..
        } => json!({
            "Form": {
                "title": title,
                "fields": fields.iter().map(form_field_to_json).collect::<Vec<_>>(),
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
        Component::Report {
            report_id,
            args,
            title,
            series,
            labels,
        } => {
            let mut r = json!({ "report_id": report_id, "args": args });
            if let Some(t) = title {
                r["title"] = json!(t);
            }
            if !series.is_empty() {
                r["series"] = json!(series);
            }
            if !labels.is_empty() {
                r["labels"] = json!(labels);
            }
            json!({ "Report": r })
        }
        Component::Diff { lines } => json!({ "Diff": { "lines": lines } }),
        Component::Sources { items } => json!({
            "Sources": {
                "items": items.iter().map(|i| json!({
                    "label": i.label,
                    "resource": i.resource,
                })).collect::<Vec<_>>(),
            }
        }),
        // Conversation components: v0.8 is frozen for the Explorer; they
        // ride as the canonical object under their PascalCase name so a
        // v0.8 host can at least see (and skip) them.
        // Every remaining kind is a conversation block
        // (`Component::is_conversation_block`): no renamed fields.
        _ => {
            debug_assert!(c.is_conversation_block());
            let mut v = serde_json::to_value(c).unwrap_or_default();
            let kind = v
                .as_object_mut()
                .and_then(|o| o.remove("kind"))
                .and_then(|k| k.as_str().map(pascal))
                .unwrap_or_default();
            json!({ kind: v })
        }
    };
    json!({ "Component": inner })
}

/// Same omit-when-unset rule as v0.9 — see `v09::form_field_to_json`. The
/// duplication is deliberate: ADR-4 keeps the two builders self-contained so a
/// schema change in one version cannot ripple into the other.
fn form_field_to_json(f: &FormField) -> Value {
    let mut o = json!({
        "name": f.name,
        "label": f.label,
        "kind": form_kind_str(f.kind),
        "required": f.required,
    });
    if let Some(p) = &f.placeholder {
        o["placeholder"] = json!(p);
    }
    if let Some(d) = &f.default_value {
        o["default"] = d.clone();
    }
    if !f.options.is_empty() {
        o["options"] = json!(f.options);
    }
    o
}

/// `key_value` → `KeyValue`.
fn pascal(snake: &str) -> String {
    snake
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn form_kind_str(k: FormFieldKind) -> &'static str {
    match k {
        FormFieldKind::String => "string",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
        FormFieldKind::Select => "select",
        FormFieldKind::Text => "text",
    }
}
