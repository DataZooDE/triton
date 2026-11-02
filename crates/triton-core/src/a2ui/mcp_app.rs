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
    Action, Chip, Component, FormField, FormFieldKind, KeyValuePair, ListItem, SelectionStyle,
    StepState, Tone, TreeNode,
};
use triton_correlation::message::{placeholders, template_parts};

/// The MCP Apps resource mime type (ext-apps spec, `RESOURCE_MIME_TYPE`).
pub const MIME_TYPE: &str = "text/html;profile=mcp-app";

/// Version of the widget data shape; the widget ignores data it does not
/// know.
pub const DATA_VERSION: u32 = 1;

/// The `tools/call` result `_meta` key that carries the widget data. The
/// host hands result `_meta` to the widget, not to the model.
pub const META_KEY: &str = "triton/widget";

/// Budgets: a card larger than these is cut, visibly ("… N more"). A
/// chat card is a summary; the full answer lives behind the open link.
/// Rows of a table, items of a list, pairs of a fact list.
pub const MAX_ROWS: usize = 50;
/// Levels of a tree.
pub const MAX_TREE_DEPTH: usize = 4;
/// Nodes of a tree, all levels.
pub const MAX_TREE_NODES: usize = 200;
/// Characters of any one text.
pub const MAX_TEXT: usize = 500;
/// Characters of a deep link (a link is not cut, so it gets more room).
pub const MAX_URL: usize = 2048;
/// Bytes of the whole widget data, serialized.
pub const MAX_BYTES: usize = 64 * 1024;

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
    cap_texts(&mut out);
    fit_bytes(out)
}

/// Every shown text at most [`MAX_TEXT`] characters, cut with an ellipsis.
/// What a control sends (a message, a template, an option value) is never
/// cut — a control whose payload is too long is no control (see
/// [`is_widget_action`], [`group`]) — so this pass only meets shown text.
/// Links and field names are not shown; a default is sent (see [`group`]).
fn cap_texts(v: &mut Value) {
    match v {
        Value::String(s) if s.chars().count() > MAX_TEXT => {
            let mut cut: String = s.chars().take(MAX_TEXT - 1).collect();
            cut.push('\u{2026}');
            *s = cut;
        }
        Value::Object(m) => m
            .iter_mut()
            .filter(|(k, _)| !matches!(k.as_str(), "url" | "name" | "field" | "default"))
            .for_each(|(_, x)| cap_texts(x)),
        Value::Array(a) => a.iter_mut().for_each(cap_texts),
        _ => {}
    }
}

/// The whole card at most [`MAX_BYTES`]: trailing body blocks go, counted
/// as the card's `more`. A card that does not fit even without its body
/// is none (the host still shows the text).
fn fit_bytes(mut out: Value) -> Option<Value> {
    let size = |v: &Value| serde_json::to_vec(v).map_or(usize::MAX, |b| b.len());
    let total = size(&out);
    if total <= MAX_BYTES {
        return Some(out);
    }
    // One pass: each block's size once (plus its comma), then keep the
    // longest prefix that fits beside the rest of the card and the `more`
    // field (at most 32 bytes).
    let body = std::mem::take(out["body"].as_array_mut()?);
    let base = size(&out) + 32;
    let mut used = base;
    let mut kept = 0;
    for b in &body {
        let s = size(b) + 1;
        if used + s > MAX_BYTES {
            break;
        }
        used += s;
        kept += 1;
    }
    if base > MAX_BYTES {
        return None;
    }
    let dropped = body.len() - kept;
    out["body"] = json!(body.into_iter().take(kept).collect::<Vec<_>>());
    out["more"] = json!(dropped);
    Some(out)
}

/// Bidi and direction marks: they can make a sent text read differently
/// on screen from what it is.
fn has_bidi(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}

/// What a control may send: non-blank, within [`MAX_TEXT`], no bidi marks.
fn sendable(t: &str) -> bool {
    !t.trim().is_empty() && t.chars().count() <= MAX_TEXT && !has_bidi(t)
}

/// What the widget opens: the core link rule's `https` branch (parsed,
/// a host, no credentials), within [`MAX_URL`].
fn openable(u: &str) -> bool {
    u.len() <= MAX_URL && u.starts_with("https://") && crate::links::is_allowed_link(u)
}

/// The first [`MAX_ROWS`] of `items`; the count of the rest goes to the
/// block's `more`.
fn first_rows(block: &mut Value, key: &str, mut items: Vec<Value>) {
    if items.len() > MAX_ROWS {
        block["more"] = json!(items.len() - MAX_ROWS);
        items.truncate(MAX_ROWS);
    }
    block[key] = json!(items);
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
/// A message or link over its budget is no control either: cut, it would
/// send or open something the producer did not mean.
pub fn is_widget_action(a: &Value) -> bool {
    match a.get("type").and_then(Value::as_str) {
        Some("send_message") => a.get("text").and_then(Value::as_str).is_some_and(sendable),
        Some("open_url") => a.get("url").and_then(Value::as_str).is_some_and(openable),
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

/// Chips as controls; a chip with a blank message would send an empty
/// turn, so it is none.
fn chip_list(chips: &[Chip]) -> Vec<Value> {
    chips
        .iter()
        .map(|c| (c, send(&c.message)))
        .filter(|(_, a)| is_widget_action(a))
        .map(|(c, a)| json!({ "label": c.label, "action": a }))
        .collect()
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
                // One header: the first (a later one, say an embed's
                // fallback, does not replace the surface's own).
                self.header.get_or_insert(h);
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
                let mut b = json!({ "type": "table", "columns": labels });
                first_rows(&mut b, "rows", rows);
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::List { title, items } => {
                let items: Vec<Value> = items.iter().map(list_item).collect();
                let mut b = json!({ "type": "list" });
                first_rows(&mut b, "items", items);
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::KeyValue { title, pairs } => {
                let pairs: Vec<Value> = pairs.iter().map(fact).collect();
                let mut b = json!({ "type": "facts" });
                first_rows(&mut b, "pairs", pairs);
                opt(&mut b, "title", title.as_deref());
                self.body.push(b);
            }
            Component::Tree {
                title,
                nodes,
                gaps_toggle,
            } => {
                let mut budget = TreeBudget::new(nodes);
                let kept = budget.nodes(nodes, 1);
                let mut b = json!({ "type": "tree", "nodes": kept });
                if budget.dropped > 0 {
                    b["more"] = json!(budget.dropped);
                }
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
                let chips = chip_list(chips);
                if chips.is_empty() {
                    return;
                }
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
                let Some(mut g) = group(None, &fields, &sub.template) else {
                    return;
                };
                opt(&mut g, "preview_label", sub.preview_label.as_deref());
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
                let Some(mut g) = group(None, fields, tpl) else {
                    return;
                };
                opt(&mut g, "preview_label", preview_label.as_deref());
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
                // A group that cannot be sent whole is none; a form with
                // none left is no block.
                let groups: Vec<Value> = groups
                    .iter()
                    .filter_map(|g| group(Some(&g.label), &g.fields, &g.send_template))
                    .collect();
                if groups.is_empty() {
                    return;
                }
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

/// A tree within [`MAX_TREE_DEPTH`] levels and [`MAX_TREE_NODES`] nodes,
/// kept breadth first: every level shown is whole, except the last one
/// that fits, which keeps its first nodes. A depth-first walk meets the
/// nodes of one level in that same order, so it admits by a per-level
/// quota.
struct TreeBudget {
    /// Nodes admitted per level (index 0 is level 1).
    quota: Vec<usize>,
    used: Vec<usize>,
    dropped: usize,
}

impl TreeBudget {
    fn new(roots: &[TreeNode]) -> Self {
        let mut per_level = Vec::new();
        fn walk(ns: &[TreeNode], l: usize, acc: &mut Vec<usize>) {
            if ns.is_empty() {
                return;
            }
            if acc.len() <= l {
                acc.push(0);
            }
            acc[l] += ns.len();
            for n in ns {
                walk(&n.children, l + 1, acc);
            }
        }
        walk(roots, 0, &mut per_level);
        let mut left = MAX_TREE_NODES;
        let quota = per_level
            .iter()
            .take(MAX_TREE_DEPTH)
            .map(|&n| {
                let q = n.min(left);
                left -= q;
                q
            })
            .collect::<Vec<_>>();
        let total: usize = per_level.iter().sum();
        let kept: usize = quota.iter().sum();
        Self {
            used: vec![0; quota.len()],
            quota,
            dropped: total - kept,
        }
    }

    fn nodes(&mut self, ns: &[TreeNode], level: usize) -> Vec<Value> {
        let l = level - 1;
        let mut out = Vec::new();
        for n in ns {
            if l < self.quota.len() && self.used[l] < self.quota[l] {
                self.used[l] += 1;
                out.push(self.node(n, level));
            }
        }
        out
    }

    fn node(&mut self, n: &TreeNode, level: usize) -> Value {
        let mut v = tree_node(n);
        let children = self.nodes(&n.children, level + 1);
        if !children.is_empty() {
            v["children"] = json!(children);
        }
        v
    }
}

/// One tree node without its children (the budget adds those).
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
    let chips = chip_list(&n.chips);
    if !chips.is_empty() {
        v["chips"] = json!(chips);
    }
    if let Some(a) = n.action.as_ref().and_then(action) {
        v["action"] = a;
        v["action_label"] = json!(n.action_caption());
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
        // An option whose value is over the text budget would send a cut
        // value: it is no option.
        let options: Vec<Value> = f
            .options
            .iter()
            .filter(|o| o.value.chars().count() <= MAX_TEXT && !has_bidi(&o.value))
            .map(|o| json!({ "label": o.label, "value": o.value }))
            .collect();
        v["options"] = json!(options);
    }
    if f.style == SelectionStyle::Radio {
        v["style"] = json!("radio");
    }
    v
}

/// A form group: its fields and its request template as parts the widget
/// fills (and previews live, under the producer's `preview_label` or the
/// widget's own caption). `None` when the template's fixed text is over
/// the text budget: the request could not be shown whole.
fn group(label: Option<&str>, fields: &[FormField], template: &str) -> Option<Value> {
    if template.chars().count() > MAX_TEXT || has_bidi(template) {
        return None;
    }
    // A form that could never be sent is none: a template field without
    // an input, two inputs with one name.
    let named = |n: &str| fields.iter().filter(|f| f.name == n).count();
    if placeholders(template).iter().any(|p| named(p) != 1)
        || fields.iter().any(|f| named(&f.name) != 1)
    {
        return None;
    }
    // A default is sent when the field is not touched: never cut.
    let bad_default = |f: &FormField| {
        f.default_value
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|d| d.chars().count() > MAX_TEXT || has_bidi(d))
    };
    if fields.iter().any(bad_default) {
        return None;
    }
    let fields: Vec<Value> = fields.iter().map(field).collect();
    // A required select left without options (all over budget) cannot be
    // answered.
    if fields.iter().any(|f| {
        f["kind"] == "select"
            && f["required"] == true
            && f["options"].as_array().is_none_or(|o| o.is_empty())
    }) {
        return None;
    }
    let mut g = json!({ "fields": fields, "template": template_parts(template) });
    opt(&mut g, "label", label);
    Some(g)
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

    /// A chip whose message is blank would send an empty turn: no chip.
    #[test]
    fn a_blank_chip_is_no_control() {
        let d = widget_data(&json!({ "surface": { "components": [
            { "kind": "header", "title": "T" },
            { "kind": "chips", "chips": [
                { "label": "blank", "message": "   " },
                { "label": "ok", "message": "Show it" } ] },
            { "kind": "chips", "chips": [ { "label": "only blank", "message": "" } ] },
            { "kind": "tree", "nodes": [ { "label": "n", "chips": [
                { "label": "blank", "message": " " } ] } ] }
        ] } }))
        .unwrap();
        assert_eq!(d["body"].as_array().unwrap().len(), 2, "{d}");
        assert_eq!(d["body"][0]["chips"].as_array().unwrap().len(), 1, "{d}");
        assert!(d["body"][1]["nodes"][0].get("chips").is_none(), "{d}");
    }

    /// The card has one header: the surface's first, not one an embed's
    /// fallback (or a second header) brings later.
    #[test]
    fn the_first_header_wins() {
        let d = widget_data(&json!({ "surface": { "components": [
            { "kind": "header", "title": "First" },
            { "kind": "embed", "url": "https://a.example/x", "title": "Open",
              "fallback": { "kind": "header", "title": "Fallback" } }
        ] } }))
        .unwrap();
        assert_eq!(d["header"]["title"], "First", "{d}");
    }
}
