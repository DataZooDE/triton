//! The canonical surface on the a2ui.org v0.9 **basic catalog** (#368).
//!
//! The MCP Apps widget bundles the official renderer (`@a2ui/lit`), which
//! ships the basic catalog only. This builder projects Triton's card
//! anatomy (header, body blocks, one primary and the secondary actions)
//! onto it: one surface, flat component list, static children, plus a data
//! model for form inputs. [`project`] also returns the card's
//! [`CardSummary`], what the model-visible card text is built from
//! (`mcp_app::card_text`).
//!
//! Controls stay stateless, as on every channel. Each is a v0.9 `event`
//! the widget handles itself (never a server round-trip):
//!
//! * [`SEND_MESSAGE`] with `context.text`: send that ONE complete message as
//!   a new user turn (`ui/message`);
//! * [`SEND_MESSAGE`] with `context.parts` (a form submit): fill the
//!   producer's template parts ([`template_parts`]) from the bound inputs
//!   `context["v:<field>"]` exactly as the server's `fill_template` would,
//!   and send the result;
//! * [`OPEN_URL`] with `context.url`: open an `https` deep link through the
//!   host (`ui/open-link`).
//!
//! The basic catalog has no table, tree, progress or chip components, so
//! those are composed from `Row`/`Column`/`Text`/`Button`. Budgets cut a
//! card visibly ("… N more"), and a cut card keeps the full text.

use serde_json::{Map, Value, json};

use super::passthrough::BASIC_CATALOG_ID;
use super::{
    Action, Chip, Component, FormField, FormFieldKind, KeyValuePair, ListItem, SelectionStyle,
    StepState, Tone, TreeNode,
};
use triton_correlation::message::{placeholders, template_parts};

/// The event a click sends as a new user message.
pub const SEND_MESSAGE: &str = "triton.send_message";
/// The event a click opens a deep link with.
pub const OPEN_URL: &str = "triton.open_url";
/// The card's surface id (one card per tool result).
pub const SURFACE_ID: &str = "triton-card";

/// Rows of a table, items of a list, pairs of a fact list.
pub const MAX_ROWS: usize = 50;
/// Levels of a tree.
pub const MAX_TREE_DEPTH: usize = 4;
/// Nodes of a tree, all levels.
pub const MAX_TREE_NODES: usize = 200;
/// Characters of any one shown text.
pub const MAX_TEXT: usize = 500;
/// Characters of a deep link (a link is not cut, so it gets more room).
pub const MAX_URL: usize = 2048;
/// Bytes of the card's messages, serialized.
pub const MAX_BYTES: usize = 64 * 1024;

/// What the model-visible card text is made of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardSummary {
    /// The header's title.
    pub title: Option<String>,
    /// The header's context line.
    pub context: Option<String>,
    /// The first row block's own title (when there is no header).
    pub block_title: Option<String>,
    /// Row names: a table row's first cell, a list item's label, a tree's
    /// top-level labels, in card order.
    pub names: Vec<String>,
    /// The card was cut by a budget: it does not show everything.
    pub cut: bool,
}

/// The card as v0.9 messages, and its summary.
#[derive(Debug, Clone)]
pub struct BasicCard {
    pub messages: Vec<Value>,
    pub summary: CardSummary,
}

/// The card for a tool result, or `None` when the result has no
/// conversation component to draw (prose only, a legacy surface, none).
pub fn project(result: &Value) -> Option<BasicCard> {
    let raw = result
        .get("surface")
        .and_then(|s| s.get("components"))
        .and_then(Value::as_array)?;
    // Per component: one malformed component drops itself, not the card.
    let components: Vec<Component> = raw.iter().filter_map(Component::parse).collect();
    let mut b = Builder::default();
    for c in &components {
        b.component(c);
    }
    b.finish()
}

// ── controls ──────────────────────────────────────────────────────────

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

/// What the widget opens: the core link rule's `https` branch (parsed, a
/// host, no credentials), within [`MAX_URL`]. Anything else would be a dead
/// button in a host frame.
fn openable(u: &str) -> bool {
    u.len() <= MAX_URL && u.starts_with("https://") && crate::links::is_allowed_link(u)
}

/// A stateless action as a v0.9 event, or `None` when it is no control (a
/// message or link the widget could not send or open whole).
fn event(a: &Action) -> Option<Value> {
    match a {
        Action::SendMessage { text } if sendable(text) => {
            Some(json!({ "event": { "name": SEND_MESSAGE, "context": { "text": text } } }))
        }
        Action::OpenUrl { url } if openable(url) => {
            Some(json!({ "event": { "name": OPEN_URL, "context": { "url": url } } }))
        }
        _ => None,
    }
}

fn send(text: &str) -> Option<Value> {
    event(&Action::SendMessage {
        text: text.to_string(),
    })
}

/// A shown text within [`MAX_TEXT`], cut with an ellipsis.
fn cap(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(MAX_TEXT - 1).collect();
    cut.push('\u{2026}');
    cut
}

fn non_blank(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

// ── the builder ───────────────────────────────────────────────────────

/// One body block: its root id, its components, its data-model entries,
/// and what it adds to the summary.
#[derive(Default)]
struct Block {
    root: String,
    comps: Vec<Value>,
    data: Map<String, Value>,
    names: Vec<String>,
    title: Option<String>,
    cut: bool,
}

#[derive(Default)]
struct Ids(usize);

impl Ids {
    fn next(&mut self) -> String {
        self.0 += 1;
        format!("c{}", self.0)
    }
}

#[derive(Default)]
struct Builder {
    ids: Ids,
    header: Option<Block>,
    title: Option<String>,
    context: Option<String>,
    body: Vec<Block>,
    /// `(primary, label, action)` in source order.
    actions: Vec<(bool, String, Value)>,
    forms: usize,
}

impl Block {
    fn push(&mut self, c: Value) -> String {
        let id = c["id"].as_str().expect("component id").to_string();
        self.comps.push(c);
        id
    }
}

impl Builder {
    fn text(&mut self, b: &mut Block, s: &str, variant: &str) -> String {
        let id = self.ids.next();
        b.push(json!({ "id": id, "component": "Text", "text": cap(s), "variant": variant }))
    }

    fn column(&mut self, b: &mut Block, children: Vec<String>) -> String {
        let id = self.ids.next();
        b.push(json!({ "id": id, "component": "Column", "children": children }))
    }

    fn row(&mut self, b: &mut Block, children: Vec<String>) -> String {
        let id = self.ids.next();
        b.push(json!({ "id": id, "component": "Row", "children": children, "align": "center" }))
    }

    fn button(&mut self, b: &mut Block, child: String, action: Value, variant: &str) -> String {
        let id = self.ids.next();
        b.push(json!({
            "id": id, "component": "Button", "child": child, "action": action, "variant": variant,
        }))
    }

    /// `child` as a borderless button when there is an action, else as is.
    fn maybe_button(&mut self, b: &mut Block, child: String, action: Option<Value>) -> String {
        match action {
            Some(a) => self.button(b, child, a, "borderless"),
            None => child,
        }
    }

    /// A block's title line (when set) before `rest`, and the "… N more"
    /// line after it when `more > 0`.
    fn push_block(
        &mut self,
        mut b: Block,
        title: Option<&str>,
        mut rest: Vec<String>,
        more: usize,
    ) {
        let mut children = Vec::new();
        if let Some(t) = non_blank(title) {
            b.title = Some(cap(t));
            children.push(self.text(&mut b, t, "h4"));
        }
        children.append(&mut rest);
        if more > 0 {
            b.cut = true;
            children.push(self.text(&mut b, &format!("\u{2026} {more} more"), "caption"));
        }
        b.root = self.column(&mut b, children);
        self.body.push(b);
    }

    fn component(&mut self, c: &Component) {
        match c {
            Component::Header {
                title,
                context,
                stamp,
            } => {
                // One header: the first (an embed's fallback does not
                // replace the surface's own).
                if self.header.is_some() {
                    return;
                }
                let mut b = Block::default();
                self.title = Some(cap(title));
                self.context = non_blank(context.as_deref()).map(cap);
                let mut children = vec![self.text(&mut b, title, "h3")];
                let line: Vec<&str> = [context.as_deref(), stamp.as_deref()]
                    .into_iter()
                    .filter_map(non_blank)
                    .collect();
                if !line.is_empty() {
                    children.push(self.text(&mut b, &line.join(" \u{b7} "), "caption"));
                }
                b.root = self.column(&mut b, children);
                self.header = Some(b);
            }
            Component::Table {
                title,
                columns,
                rows,
            } => {
                let mut b = Block::default();
                let heads: Vec<String> = columns
                    .iter()
                    .map(|c| self.text(&mut b, &c.label, "caption"))
                    .collect();
                let mut lines = vec![self.row(&mut b, heads)];
                for r in rows.iter().take(MAX_ROWS) {
                    let cells: Vec<String> = columns
                        .iter()
                        .map(|c| r.cells.get(&c.key).cloned().unwrap_or_default())
                        .collect();
                    b.names
                        .push(cap(cells.first().map(String::as_str).unwrap_or("")));
                    let mut ids: Vec<String> =
                        cells.iter().map(|t| self.text(&mut b, t, "body")).collect();
                    if let Some(first) = ids.first_mut() {
                        let a = r.action.as_ref().and_then(event);
                        *first = self.maybe_button(&mut b, first.clone(), a);
                    }
                    lines.push(self.row(&mut b, ids));
                }
                let more = rows.len().saturating_sub(MAX_ROWS);
                self.push_block(b, title.as_deref(), lines, more);
            }
            Component::List { title, items } => {
                let mut b = Block::default();
                let lines: Vec<String> = items
                    .iter()
                    .take(MAX_ROWS)
                    .map(|it| self.list_item(&mut b, it))
                    .collect();
                let more = items.len().saturating_sub(MAX_ROWS);
                self.push_block(b, title.as_deref(), lines, more);
            }
            Component::KeyValue { title, pairs } => {
                let mut b = Block::default();
                let lines: Vec<String> = pairs
                    .iter()
                    .take(MAX_ROWS)
                    .map(|p| self.fact(&mut b, p))
                    .collect();
                let more = pairs.len().saturating_sub(MAX_ROWS);
                self.push_block(b, title.as_deref(), lines, more);
            }
            Component::Tree { title, nodes, .. } => {
                let mut b = Block::default();
                let mut budget = TreeBudget::new(nodes);
                let mut lines = Vec::new();
                budget.walk(nodes, 1, &mut |level, n| {
                    if level == 1 {
                        b.names.push(cap(&n.label));
                    }
                    lines.push(self.tree_node(&mut b, n, level));
                });
                let more = budget.dropped;
                self.push_block(b, title.as_deref(), lines, more);
            }
            Component::Notice { tone, title, text } => {
                let mut b = Block::default();
                let glyph = match tone {
                    Tone::Info => "\u{2139}\u{fe0f}",
                    Tone::Warn | Tone::Attention => "\u{26a0}\u{fe0f}",
                };
                let head = match non_blank(title.as_deref()) {
                    Some(t) => format!("{glyph} {t}"),
                    None => glyph.to_string(),
                };
                let children = vec![
                    self.text(&mut b, &head, "h4"),
                    self.text(&mut b, text, "body"),
                ];
                let col = self.column(&mut b, children);
                let id = self.ids.next();
                b.root = b.push(json!({ "id": id, "component": "Card", "child": col }));
                self.body.push(b);
            }
            Component::Progress {
                title,
                steps,
                fraction,
            } => {
                let mut b = Block::default();
                let mut lines: Vec<String> = steps
                    .iter()
                    .map(|s| {
                        let glyph = match s.state {
                            StepState::Done => "\u{2713}",
                            StepState::Active => "\u{25b8}",
                            StepState::Pending => "\u{25cb}",
                        };
                        self.text(&mut b, &format!("{glyph} {}", s.label), "body")
                    })
                    .collect();
                if let Some(f) = fraction {
                    let pct = (f.clamp(0.0, 1.0) * 100.0).round();
                    lines.push(self.text(&mut b, &format!("{pct} %"), "caption"));
                }
                self.push_block(b, title.as_deref(), lines, 0);
            }
            Component::Chips { prompt, chips } => {
                let mut b = Block::default();
                let Some(row) = self.chip_row(&mut b, chips) else {
                    return;
                };
                self.push_block(b, prompt.as_deref(), vec![row], 0);
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
                let mut b = Block::default();
                let Some(g) = self.group(&mut b, &fields, &sub.template, &sub.label) else {
                    return;
                };
                self.push_block(b, None, vec![g], 0);
            }
            Component::Form {
                title,
                fields,
                submit_label,
                send_template: Some(tpl),
                ..
            } => {
                let mut b = Block::default();
                let Some(g) = self.group(&mut b, fields, tpl, submit_label) else {
                    return;
                };
                self.push_block(b, Some(title), vec![g], 0);
            }
            Component::FormGroups {
                title,
                groups,
                submit_label,
                note,
            } => {
                // A group that cannot be sent whole is none; a form with
                // none left is no block. One tab per group.
                let mut b = Block::default();
                let mut tabs = Vec::new();
                for g in groups {
                    if let Some(child) =
                        self.group(&mut b, &g.fields, &g.send_template, submit_label)
                    {
                        tabs.push(json!({ "title": cap(&g.label), "child": child }));
                    }
                }
                if tabs.is_empty() {
                    return;
                }
                let id = self.ids.next();
                let mut rest = vec![b.push(json!({ "id": id, "component": "Tabs", "tabs": tabs }))];
                if let Some(n) = non_blank(note.as_deref()) {
                    rest.push(self.text(&mut b, n, "caption"));
                }
                self.push_block(b, title.as_deref(), rest, 0);
            }
            Component::Button {
                label,
                action: Some(a),
                primary,
                ..
            } => {
                if let Some(a) = event(a) {
                    self.actions.push((*primary, label.clone(), a));
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
                if let Some(a) = event(&Action::OpenUrl { url: url.clone() }) {
                    self.actions.push((false, title.clone(), a));
                }
            }
            // Prose rides the host's text content; legacy controls (tool
            // re-invocation) have no stateless meaning here.
            _ => {}
        }
    }

    fn list_item(&mut self, b: &mut Block, it: &ListItem) -> String {
        b.names.push(cap(&it.label));
        let mut lines = vec![self.text(b, &it.label, "body")];
        if let Some(d) = non_blank(it.detail.as_deref()) {
            lines.push(self.text(b, d, "caption"));
        }
        let col = self.column(b, lines);
        let Some(a) = it.action.as_ref().and_then(event) else {
            return col;
        };
        // A separate control at the end of the row, or the row itself.
        match non_blank(it.action_label.as_deref()) {
            Some(l) => {
                let label = self.text(b, l, "body");
                let btn = self.button(b, label, a, "default");
                self.row(b, vec![col, btn])
            }
            None => self.button(b, col, a, "borderless"),
        }
    }

    fn fact(&mut self, b: &mut Block, p: &KeyValuePair) -> String {
        let value = match non_blank(p.before.as_deref()) {
            Some(before) => format!("{before} \u{2192} {}", p.value),
            None => p.value.clone(),
        };
        let l = self.text(b, &p.label, "caption");
        let v = self.text(b, &value, "body");
        self.row(b, vec![l, v])
    }

    fn chip_row(&mut self, b: &mut Block, chips: &[Chip]) -> Option<String> {
        let buttons: Vec<String> = chips
            .iter()
            .filter_map(|c| Some((c, send(&c.message)?)))
            .map(|(c, a)| {
                let label = self.text(b, &c.label, "body");
                self.button(b, label, a, "default")
            })
            .collect();
        (!buttons.is_empty()).then(|| self.row(b, buttons))
    }

    /// One tree node: its label line (indented per level, with counts and a
    /// gap mark) as a button when it has an action, and its chips.
    fn tree_node(&mut self, b: &mut Block, n: &TreeNode, level: usize) -> String {
        let counts: Vec<String> = n
            .counts
            .iter()
            .map(|c| format!("{} {}", c.value, c.label).trim().to_string())
            .collect();
        let mut line = "\u{2003}".repeat(level - 1);
        if n.gap {
            line.push_str("\u{26a0}\u{fe0f} ");
        }
        line.push_str(&n.label);
        if !counts.is_empty() {
            line.push_str(&format!(" \u{2014} {}", counts.join(" \u{b7} ")));
        }
        let text = self.text(b, &line, if level == 1 { "body" } else { "caption" });
        let a = n.action.as_ref().and_then(event);
        let head = self.maybe_button(b, text, a);
        match self.chip_row(b, &n.chips) {
            Some(chips) => self.column(b, vec![head, chips]),
            None => head,
        }
    }

    /// A form group: its inputs bound to `/f<n>/<name>` and a submit that
    /// sends the template's parts and the bound values. `None` when the
    /// request could not be sent whole (see the checks).
    fn group(
        &mut self,
        b: &mut Block,
        fields: &[FormField],
        template: &str,
        submit_label: &str,
    ) -> Option<String> {
        if template.chars().count() > MAX_TEXT || has_bidi(template) {
            return None;
        }
        // A template field without an input, two inputs with one name.
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
        // An option whose value is over the budget would send a cut value:
        // it is no option. A required select left without options cannot be
        // answered.
        let options = |f: &FormField| -> Vec<Value> {
            f.options
                .iter()
                .filter(|o| o.value.chars().count() <= MAX_TEXT && !has_bidi(&o.value))
                .map(|o| json!({ "label": cap(&o.label), "value": o.value }))
                .collect()
        };
        if fields
            .iter()
            .any(|f| matches!(f.kind, FormFieldKind::Select) && f.required && options(f).is_empty())
        {
            return None;
        }
        let form = format!("f{}", self.forms);
        self.forms += 1;
        let mut model = Map::new();
        let mut context = Map::new();
        context.insert("parts".into(), json!(template_parts(template).to_string()));
        let mut children = Vec::new();
        for (k, f) in fields.iter().enumerate() {
            // A generated key, never the producer's name: the renderer
            // refuses some names as path segments (`constructor`) and reads
            // others as pointers (`a/b`). The submit maps names to keys.
            let key = format!("i{k}");
            let path = format!("/{form}/{key}");
            context.insert(format!("v:{}", f.name), json!({ "path": path }));
            let label = match f.required {
                true => format!("{} *", f.label),
                false => f.label.clone(),
            };
            let id = self.ids.next();
            let (comp, init) = match f.kind {
                FormFieldKind::Select => {
                    let init = f
                        .default_value
                        .as_ref()
                        .and_then(Value::as_str)
                        .map_or(json!([]), |d| json!([d]));
                    let mut c = json!({
                        "id": id, "component": "ChoicePicker", "label": cap(&label),
                        "options": options(f), "value": { "path": path },
                        "variant": "mutuallyExclusive",
                    });
                    if f.style == SelectionStyle::Radio {
                        c["displayStyle"] = json!("checkbox");
                    }
                    (c, init)
                }
                // Three-state, as on every other channel: unanswered until
                // chosen, never a silent `false`.
                FormFieldKind::Boolean => (
                    json!({
                        "id": id, "component": "ChoicePicker", "label": cap(&label),
                        "options": [ { "label": "Yes", "value": "true" },
                                     { "label": "No", "value": "false" } ],
                        "value": { "path": path }, "variant": "mutuallyExclusive",
                    }),
                    f.default_value
                        .as_ref()
                        .and_then(Value::as_bool)
                        .map_or(json!([]), |b| json!([b.to_string()])),
                ),
                FormFieldKind::Text | FormFieldKind::String | FormFieldKind::Integer => {
                    let variant = match f.kind {
                        FormFieldKind::Text => "longText",
                        FormFieldKind::Integer => "number",
                        _ => "shortText",
                    };
                    let init = match &f.default_value {
                        Some(Value::String(s)) => json!(s),
                        Some(Value::Null) | None => json!(""),
                        Some(v) => json!(v.to_string()),
                    };
                    (
                        json!({ "id": id, "component": "TextField", "label": cap(&label),
                                "value": { "path": path }, "variant": variant }),
                        init,
                    )
                }
            };
            model.insert(key, init);
            children.push(b.push(comp));
        }
        let label = self.text(b, submit_label, "body");
        let action = json!({ "event": { "name": SEND_MESSAGE, "context": context } });
        children.push(self.button(b, label, action, "primary"));
        b.data.insert(form, Value::Object(model));
        Some(self.column(b, children))
    }

    fn finish(mut self) -> Option<BasicCard> {
        if self.header.is_none() && self.body.is_empty() && self.actions.is_empty() {
            return None;
        }
        // One anatomy: the primary action leads, the others follow in order.
        self.actions.sort_by_key(|(primary, _, _)| !*primary);
        let mut bar = Block::default();
        let mut buttons = Vec::new();
        for (primary, label, a) in std::mem::take(&mut self.actions) {
            let l = self.text(&mut bar, &label, "body");
            let variant = if primary { "primary" } else { "default" };
            buttons.push(self.button(&mut bar, l, a, variant));
        }
        if !buttons.is_empty() {
            bar.root = self.row(&mut bar, buttons);
        }

        // The byte budget: trailing body blocks go, counted as "… N more".
        // A block costs its components, its data-model entries and its id
        // in the card column's `children` (quoted, with a comma).
        let size = |b: &Block| {
            serde_json::to_vec(&b.comps).map_or(usize::MAX, |v| v.len())
                + serde_json::to_vec(&b.data).map_or(usize::MAX, |v| v.len())
                + b.root.len()
                + 3
        };
        let fixed = self.header.as_ref().map_or(0, size) + size(&bar) + 1024;
        if fixed > MAX_BYTES {
            return None;
        }
        let mut used = fixed;
        let mut kept = 0;
        for blk in &self.body {
            let s = size(blk);
            if used + s > MAX_BYTES {
                break;
            }
            used += s;
            kept += 1;
        }
        let dropped = self.body.len() - kept;
        self.body.truncate(kept);

        let mut summary = CardSummary {
            title: self.title.take(),
            context: self.context.take(),
            cut: dropped > 0,
            ..CardSummary::default()
        };
        let mut comps = Vec::new();
        let mut data = Map::new();
        let mut children = Vec::new();
        for blk in self.header.take().into_iter().chain(self.body.drain(..)) {
            summary.cut |= blk.cut;
            if !blk.names.is_empty() {
                summary.block_title = summary.block_title.or(blk.title);
                summary.names.extend(blk.names);
            }
            children.push(blk.root);
            comps.extend(blk.comps);
            data.extend(blk.data);
        }
        if dropped > 0 {
            let mut more = Block::default();
            children.push(self.text(&mut more, &format!("\u{2026} {dropped} more"), "caption"));
            comps.extend(more.comps);
        }
        if !bar.root.is_empty() {
            children.push(bar.root);
            comps.extend(bar.comps);
        }
        comps.push(json!({ "id": "card-col", "component": "Column", "children": children }));
        comps.push(json!({ "id": "root", "component": "Card", "child": "card-col" }));

        let mut messages = vec![
            json!({ "version": "v0.9", "createSurface": {
                "surfaceId": SURFACE_ID, "catalogId": BASIC_CATALOG_ID,
            }}),
            json!({ "version": "v0.9", "updateComponents": {
                "surfaceId": SURFACE_ID, "components": comps,
            }}),
        ];
        if !data.is_empty() {
            messages.push(json!({ "version": "v0.9", "updateDataModel": {
                "surfaceId": SURFACE_ID, "path": "/", "value": data,
            }}));
        }
        Some(BasicCard { messages, summary })
    }
}

/// A tree within [`MAX_TREE_DEPTH`] levels and [`MAX_TREE_NODES`] nodes,
/// kept breadth first: every level shown is whole, except the last one that
/// fits, which keeps its first nodes. A depth-first walk meets the nodes of
/// one level in that same order, so it admits by a per-level quota.
struct TreeBudget {
    quota: Vec<usize>,
    used: Vec<usize>,
    dropped: usize,
}

impl TreeBudget {
    fn new(roots: &[TreeNode]) -> Self {
        fn count(ns: &[TreeNode], l: usize, acc: &mut Vec<usize>) {
            if ns.is_empty() {
                return;
            }
            if acc.len() <= l {
                acc.push(0);
            }
            acc[l] += ns.len();
            for n in ns {
                count(&n.children, l + 1, acc);
            }
        }
        let mut per_level = Vec::new();
        count(roots, 0, &mut per_level);
        let mut left = MAX_TREE_NODES;
        let quota: Vec<usize> = per_level
            .iter()
            .take(MAX_TREE_DEPTH)
            .map(|&n| {
                let q = n.min(left);
                left -= q;
                q
            })
            .collect();
        let total: usize = per_level.iter().sum();
        let kept: usize = quota.iter().sum();
        Self {
            used: vec![0; quota.len()],
            quota,
            dropped: total - kept,
        }
    }

    /// Visit the admitted nodes depth first, with their level (1-based).
    fn walk(&mut self, ns: &[TreeNode], level: usize, f: &mut impl FnMut(usize, &TreeNode)) {
        let l = level - 1;
        for n in ns {
            if l < self.quota.len() && self.used[l] < self.quota[l] {
                self.used[l] += 1;
                f(level, n);
                self.walk(&n.children, level + 1, f);
            }
        }
    }
}
