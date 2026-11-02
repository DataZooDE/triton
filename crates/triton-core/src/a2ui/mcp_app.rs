//! The canonical surface as an **MCP Apps** widget (ChatGPT, Claude).
//!
//! The third conversation mapping beside Gemini's A2UI v0.9 (`ge.rs`) and
//! the Teams Adaptive Card (`triton-chat-msteams::conversation_card`). An
//! MCP host that renders MCP Apps (SEP-1865, extension
//! `io.modelcontextprotocol/ui`, spec 2026-01-26) loads ONE static HTML
//! resource for a tool and hands it each `tools/call` result. So the
//! mapping has two halves:
//!
//! * [`widget_html`] — the static, self-contained widget (inline CSS and
//!   JS, no network, no secrets) served at [`resource_uri`] with
//!   [`MIME_TYPE`];
//! * [`widget_data`] — the per-answer data it draws, derived from the
//!   canonical surface: the card anatomy (header, body blocks, one primary
//!   and the secondary actions, attention tone).
//!
//! Actions stay stateless, as on every channel: a click sends ONE complete
//! new user message through the host (`ui/message`, or ChatGPT's
//! `sendFollowUpMessage`), or opens an `https` deep link through the host
//! (`ui/open-link`, `openExternal`). A form composes its message on the
//! client from the producer's template, parsed into parts
//! ([`triton_correlation::message::template_parts`]) so the client fills it
//! exactly as [`triton_correlation::message::fill_template`] would. Nothing
//! is signed: the message is no more privileged than one the user types,
//! and it reaches the agent as a new turn of the host's conversation.
//!
//! Prose (`text`, `narration`) stays out of the widget: the host shows the
//! tool's text content, and the card must not say it twice.

use serde_json::{Value, json};

use super::{
    Action, Component, FormField, FormFieldKind, KeyValuePair, ListItem, SelectionStyle, StepState,
    Tone, TreeNode,
};
use triton_correlation::message::{preview_template, template_parts};

/// The MCP Apps resource mime type (ext-apps spec, `RESOURCE_MIME_TYPE`).
pub const MIME_TYPE: &str = "text/html;profile=mcp-app";

/// Version of the widget data shape; the widget ignores data it does not
/// know.
pub const DATA_VERSION: u32 = 1;

const HTML: &str = include_str!("mcp_app_widget.html");

/// The static widget page.
pub fn widget_html() -> &'static str {
    HTML
}

/// The widget's `ui://` URI. It names a hash of the page, so a host that
/// caches resources by URI (ChatGPT does) picks up a changed widget.
pub fn resource_uri() -> &'static str {
    static URI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URI.get_or_init(|| {
        // FNV-1a, 64 bit: a cache key, not a security property.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in HTML.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("ui://triton/answer-card-{:08x}.html", h >> 32)
    })
}

/// True for any revision of the widget's URI: a host that listed the tools
/// before an upgrade still gets the current page.
pub fn is_widget_uri(uri: &str) -> bool {
    uri.strip_prefix("ui://triton/answer-card")
        .is_some_and(|rest| rest.ends_with(".html") && !rest.contains('/'))
}

/// The widget's data for a tool result, or `None` when the result has no
/// conversation component to draw (prose only, a legacy surface, no
/// surface at all).
pub fn widget_data(result: &Value) -> Option<Value> {
    let raw = result
        .get("surface")
        .and_then(|s| s.get("components"))
        .and_then(Value::as_array)?;
    // Per component, as the GE mapping does: one malformed component drops
    // itself, not the card.
    let components: Vec<Component> = raw.iter().filter_map(Component::parse).collect();
    let mut card = Card::default();
    for c in &components {
        card.component(c);
    }
    if card.header.is_none() && card.body.is_empty() && card.actions.is_empty() {
        return None;
    }
    // One anatomy: the primary action leads, the others follow in order.
    card.actions
        .sort_by_key(|a| !a["primary"].as_bool().unwrap_or(false));
    let mut out = json!({
        "version": DATA_VERSION,
        "body": card.body,
        "actions": card.actions,
    });
    if let Some(h) = card.header {
        out["header"] = h;
    }
    if let Some(t) = card.tone {
        out["tone"] = json!(t);
    }
    Some(out)
}

#[derive(Default)]
struct Card {
    header: Option<Value>,
    body: Vec<Value>,
    actions: Vec<Value>,
    tone: Option<&'static str>,
}

fn opt(v: &mut Value, key: &str, s: Option<&str>) {
    if let Some(s) = s.filter(|s| !s.trim().is_empty()) {
        v[key] = json!(s);
    }
}

/// The widget's own link rule: it opens `https` links only (through the
/// host), so any other link — even one the core link rule admits, such as
/// an `http://` link to a configured local host — would be a dead button.
pub fn is_widget_action(a: &Value) -> bool {
    match a.get("type").and_then(Value::as_str) {
        Some("send_message") => a
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty()),
        Some("open_url") => a
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| u.starts_with("https://")),
        _ => false,
    }
}

fn action(a: &Action) -> Option<Value> {
    Some(json!(a)).filter(is_widget_action)
}

fn send(text: &str) -> Value {
    json!(Action::SendMessage {
        text: text.to_string()
    })
}

impl Card {
    fn component(&mut self, c: &Component) {
        match c {
            Component::Header {
                title,
                context,
                stamp,
            } => {
                let mut h = json!({ "title": title });
                opt(&mut h, "context", context.as_deref());
                opt(&mut h, "stamp", stamp.as_deref());
                self.header = Some(h);
            }
            Component::Table {
                title,
                columns,
                rows,
            } => {
                let rows: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        let cells: Vec<String> = columns
                            .iter()
                            .map(|c| r.cells.get(&c.key).cloned().unwrap_or_default())
                            .collect();
                        let mut row = json!({ "cells": cells });
                        if let Some(a) = r.action.as_ref().and_then(action) {
                            row["action"] = a;
                        }
                        row
                    })
                    .collect();
                let labels: Vec<&str> = columns.iter().map(|c| c.label.as_str()).collect();
                let mut b = json!({ "type": "table", "columns": labels, "rows": rows });
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::List { title, items } => {
                let items: Vec<Value> = items.iter().map(list_item).collect();
                let mut b = json!({ "type": "list", "items": items });
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::KeyValue { title, pairs } => {
                let pairs: Vec<Value> = pairs.iter().map(fact).collect();
                let mut b = json!({ "type": "facts", "pairs": pairs });
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::Tree {
                title,
                nodes,
                gaps_toggle,
            } => {
                let nodes: Vec<Value> = nodes.iter().map(tree_node).collect();
                let mut b = json!({ "type": "tree", "nodes": nodes });
                opt(&mut b, "title", title.as_deref());
                opt(&mut b, "gaps_toggle", gaps_toggle.as_deref());
                self.body.push(b);
            }
            Component::Notice { tone, title, text } => {
                let tone = match tone {
                    Tone::Info => "info",
                    Tone::Warn => "warn",
                    Tone::Attention => "attention",
                };
                if tone == "attention" {
                    self.tone = Some("attention");
                }
                let mut b = json!({ "type": "notice", "tone": tone, "text": text });
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::Progress {
                title,
                steps,
                fraction,
            } => {
                let steps: Vec<Value> = steps
                    .iter()
                    .map(|s| {
                        let state = match s.state {
                            StepState::Done => "done",
                            StepState::Active => "active",
                            StepState::Pending => "pending",
                        };
                        json!({ "label": s.label, "state": state })
                    })
                    .collect();
                let mut b = json!({ "type": "progress", "steps": steps });
                opt(&mut b, "title", title.as_deref());
                if let Some(f) = fraction {
                    b["fraction"] = json!(f.clamp(0.0, 1.0));
                }
                self.body.push(b);
            }
            Component::Chips { prompt, chips } => {
                let chips: Vec<Value> = chips
                    .iter()
                    .map(|c| json!({ "label": c.label, "action": send(&c.message) }))
                    .collect();
                let mut b = json!({ "type": "chips", "chips": chips });
                opt(&mut b, "prompt", prompt.as_deref());
                self.body.push(b);
            }
            Component::Selection {
                prompt,
                options,
                args_key,
                style,
                submit: Some(sub),
                ..
            } => {
                let fields = [FormField {
                    name: args_key.clone(),
                    label: prompt.clone(),
                    kind: FormFieldKind::Select,
                    required: true,
                    placeholder: None,
                    default_value: None,
                    options: options.clone(),
                    style: *style,
                }];
                let mut g = group(None, &fields, &sub.template);
                if let Some(label) = &sub.preview_label {
                    g["preview"] = preview(label, &fields, &sub.template);
                }
                self.body.push(json!({
                    "type": "form",
                    "submit_label": sub.label,
                    "groups": [g],
                }));
            }
            Component::Form {
                title,
                fields,
                submit_label,
                send_template: Some(tpl),
                preview_label,
                ..
            } => {
                let mut g = group(None, fields, tpl);
                if let Some(label) = preview_label {
                    g["preview"] = preview(label, fields, tpl);
                }
                let mut b = json!({
                    "type": "form",
                    "submit_label": submit_label,
                    "groups": [g],
                });
                opt(&mut b, "title", Some(title.as_str()));
                self.body.push(b);
            }
            Component::FormGroups {
                title,
                groups,
                submit_label,
                note,
            } => {
                let groups: Vec<Value> = groups
                    .iter()
                    .map(|g| group(Some(&g.label), &g.fields, &g.send_template))
                    .collect();
                let mut b = json!({
                    "type": "form",
                    "submit_label": submit_label,
                    "groups": groups,
                });
                opt(&mut b, "title", title.as_deref());
                opt(&mut b, "note", note.as_deref());
                self.body.push(b);
            }
            Component::Button {
                label,
                action: Some(a),
                primary,
                ..
            } => {
                if let Some(a) = action(a) {
                    self.actions.push(json!({
                        "label": label,
                        "primary": primary,
                        "action": a,
                    }));
                }
            }
            // The widget does not frame a page (a host would need the page's
            // origin in its frame CSP, which Claude restricts): the fallback
            // inline, and the page as a link in the action bar.
            Component::Embed {
                url,
                title,
                fallback,
                ..
            } => {
                self.component(fallback);
                if let Some(a) = action(&Action::OpenUrl { url: url.clone() }) {
                    self.actions.push(json!({
                        "label": title,
                        "primary": false,
                        "action": a,
                    }));
                }
            }
            // Prose rides the host's text content; legacy controls (tool
            // re-invocation) have no stateless meaning here.
            _ => {}
        }
    }
}

fn list_item(it: &ListItem) -> Value {
    let mut v = json!({ "label": it.label });
    opt(&mut v, "detail", it.detail.as_deref());
    if let Some(a) = it.action.as_ref().and_then(action) {
        v["action"] = a;
        opt(&mut v, "action_label", it.action_label.as_deref());
    }
    v
}

fn fact(p: &KeyValuePair) -> Value {
    let mut v = json!({ "label": p.label, "value": p.value });
    opt(&mut v, "before", p.before.as_deref());
    v
}

fn tree_node(n: &TreeNode) -> Value {
    let mut v = json!({ "label": n.label });
    let counts: Vec<String> = n
        .counts
        .iter()
        .map(|c| format!("{} {}", c.value, c.label).trim().to_string())
        .collect();
    if !counts.is_empty() {
        v["count"] = json!(counts.join(" \u{b7} "));
    }
    if n.gap {
        v["gap"] = json!(true);
    }
    if n.expanded {
        v["expanded"] = json!(true);
    }
    if !n.chips.is_empty() {
        let chips: Vec<Value> = n
            .chips
            .iter()
            .map(|c| json!({ "label": c.label, "action": send(&c.message) }))
            .collect();
        v["chips"] = json!(chips);
    }
    if let Some(a) = n.action.as_ref().and_then(action) {
        v["action"] = a;
        v["action_label"] = json!(n.action_caption());
    }
    if !n.children.is_empty() {
        let children: Vec<Value> = n.children.iter().map(tree_node).collect();
        v["children"] = json!(children);
    }
    v
}

fn field(f: &FormField) -> Value {
    let kind = match f.kind {
        FormFieldKind::Select => "select",
        FormFieldKind::Text => "text",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
        FormFieldKind::String => "string",
    };
    let mut v = json!({ "name": f.name, "label": f.label, "kind": kind });
    if f.required {
        v["required"] = json!(true);
    }
    opt(&mut v, "placeholder", f.placeholder.as_deref());
    if let Some(d) = &f.default_value {
        v["default"] = d.clone();
    }
    if !f.options.is_empty() {
        let options: Vec<Value> = f
            .options
            .iter()
            .map(|o| json!({ "label": o.label, "value": o.value }))
            .collect();
        v["options"] = json!(options);
    }
    if f.style == SelectionStyle::Radio {
        v["style"] = json!("radio");
    }
    v
}

fn group(label: Option<&str>, fields: &[FormField], template: &str) -> Value {
    let fields: Vec<Value> = fields.iter().map(field).collect();
    let mut g = json!({ "fields": fields, "template": template_parts(template) });
    opt(&mut g, "label", label);
    g
}

/// "Will be sent as …" — only when the producer names the caption: the
/// composed request is plumbing, not content.
fn preview(label: &str, fields: &[FormField], template: &str) -> Value {
    let text = preview_template(template, |name| {
        fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.label.clone())
    });
    json!({ "label": label, "text": text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uri_names_the_page_revision() {
        let uri = resource_uri();
        assert!(is_widget_uri(uri), "{uri}");
        assert!(is_widget_uri("ui://triton/answer-card-00000000.html"));
        assert!(!is_widget_uri("ui://triton/runtime.html"));
        assert!(!is_widget_uri("ui://triton/answer-card/../x.html"));
        assert!(!is_widget_uri("ui://peacock/answer-card.html"));
    }

    #[test]
    fn an_unsafe_link_never_becomes_an_action() {
        let d = widget_data(&json!({ "surface": { "components": [
            { "kind": "header", "title": "T" },
            { "kind": "button", "label": "x",
              "action": { "type": "open_url", "url": "javascript:alert(1)" } },
            { "kind": "list", "items": [ { "label": "r",
              "action": { "type": "open_url", "url": "file:///etc/passwd" } } ] }
        ] } }))
        .unwrap();
        assert_eq!(d["actions"], json!([]));
        assert!(d["body"][0]["items"][0].get("action").is_none(), "{d}");
    }
}
