//! Adaptive Card elements for the conversation components: List,
//! Table, Chips, Tree, Notice, Progress, KeyValue, Embed, plus the
//! template-carrying Selection and Form and the stateless-action Button.
//!
//! Every interactive element follows the one product rule: **a click sends
//! one complete new request.** A send-message control carries a signed
//! correlation token over `(SEND_MESSAGE_TOOL, {m})` (fixed text) or
//! `(SEND_MESSAGE_TOOL, {tpl})` (a template the callback fills from the card
//! inputs); `open_url` is a plain `Action.OpenUrl`. Signing stays in
//! `lib.rs` (it holds the key) and is passed in as `sign`.
//!
//! Teams-only affordances, both client-side so nothing round-trips:
//! * a Tree node toggles its children with `Action.ToggleVisibility`;
//! * the Tree's "only gaps" toggle flips every node whose subtree holds no
//!   gap.

use serde_json::{Value, json};
use triton_core::a2ui::{
    Action, Chip, Component, FormField, FormFieldKind, FormGroup, KeyValuePair, ListItem,
    ProgressStep, StepState, Surface, TableColumn, TableRow, Tone, TreeNode,
};
use triton_correlation::message::{
    SEND_MESSAGE_TOOL, preview_template, rename_placeholders, required_placeholders, template_args,
    text_args,
};

use crate::surface_mapper::{self, ACTION_VERB, CardChrome, InteractiveSpec, TOKEN_DATA_KEY};

/// Sentinel tool for the Embed "open" button: a `task/fetch` whose token
/// carries `{u: url, t: title}`; the handler answers with a URL task module.
pub const OPEN_EMBED_TOOL: &str = "__open_embed";

/// Signs `(tool, args)` into a correlation token; `None` drops the control.
pub type Sign<'a> = dyn FnMut(&str, &Value) -> Option<String> + 'a;

/// Compose the Adaptive Card `content` for a dispatch result, or `None` when
/// a plain-text reply serves (no controls, dashboard, image, rich markdown
/// or conversation component). Key-free: `sign` mints each control's
/// correlation token (the adapter holds the key), which is what lets the
/// golden tests render the exact card the webhook sends.
pub fn card_for_result(
    result: &Value,
    image_url: Option<&str>,
    chart: Option<&Value>,
    chrome: &CardChrome,
    sign: &mut Sign<'_>,
) -> Option<Value> {
    let specs = surface_mapper::interactive_from_result(result);
    let dashboard = surface_mapper::dashboard_from_result(result);
    let text = match surface_mapper::try_render_surface(result) {
        Some(Ok(r)) => r.text,
        _ => String::new(),
    };
    // A reply with headings or tables gets a card even with no controls:
    // the card renders them as compact native elements, where Teams' own
    // Markdown renderer would use title-size headings (md_card). Conversation
    // components always get a card.
    let elements = match triton_core::a2ui::extract_surface(result) {
        Ok(surface) => elements(&surface, sign),
        Err(_) => Vec::new(),
    };
    let rich = crate::md_card::is_rich_markdown(&text) || !elements.is_empty();
    if specs.is_empty() && dashboard.is_none() && image_url.is_none() && !rich {
        return None;
    }
    let signed: Vec<(InteractiveSpec, String)> = specs
        .into_iter()
        .filter_map(|spec| {
            // A deep link needs no callback, so no token.
            if matches!(spec, InteractiveSpec::OpenUrl { .. }) {
                return Some((spec, String::new()));
            }
            let token = sign(spec.tool(), &spec.base_args())?;
            Some((spec, token))
        })
        .collect();
    // Every control dropped, no dashboard, no image → nothing to put on a
    // card; fall back to text.
    if signed.is_empty() && dashboard.is_none() && image_url.is_none() && !rich {
        return None;
    }
    let mut card = surface_mapper::build_adaptive_card_with(
        &text,
        dashboard.as_ref(),
        &signed,
        image_url,
        chart,
        chrome,
        &elements,
    );
    mark_primary(&mut card, result);
    Some(card)
}

/// One anatomy: the surface's primary button leads the card's action bar
/// in the host's primary style (`positive`); the secondary ones follow in
/// surface order.
fn mark_primary(card: &mut Value, result: &Value) {
    let Ok(surface) = triton_core::a2ui::extract_surface(result) else {
        return;
    };
    let primary: Vec<&str> = surface
        .components
        .iter()
        .filter_map(|c| match c {
            Component::Button {
                label,
                primary: true,
                ..
            } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    let Some(actions) = card.get_mut("actions").and_then(Value::as_array_mut) else {
        return;
    };
    if primary.is_empty() {
        return;
    }
    let (mut first, rest): (Vec<Value>, Vec<Value>) = actions
        .drain(..)
        .partition(|a| primary.contains(&a["title"].as_str().unwrap_or_default()));
    for a in &mut first {
        a["style"] = json!("positive");
    }
    first.extend(rest);
    *actions = first;
}

/// The components this module renders: the conversation set minus
/// action buttons, which ride the card's top-level `actions` bar
/// (`surface_mapper::interactive_from_result`).
pub fn is_conversation(c: &Component) -> bool {
    c.is_conversation() && !matches!(c, Component::Button { .. })
}

/// The card body elements for every conversation component, in surface
/// order.
pub fn elements(surface: &Surface, sign: &mut Sign<'_>) -> Vec<Value> {
    let mut b = Builder {
        sign,
        n: 0,
        forms: 0,
    };
    let mut out = Vec::new();
    for c in &surface.components {
        if is_conversation(c) {
            out.extend(b.component(c));
        }
    }
    out
}

struct Builder<'s, 'a> {
    sign: &'s mut Sign<'a>,
    /// Element-id counter (tree nodes, toggle targets).
    n: usize,
    /// Template-component counter: input ids are card-global in Teams, so
    /// each template component's inputs live under `t<k>.` (F9).
    forms: usize,
}

fn heading(text: &str) -> Value {
    json!({ "type": "TextBlock", "text": text, "weight": "Bolder", "wrap": true })
}

/// `gather`: a template submit gathers (and validates) the card inputs; a
/// fixed message must not (`associatedInputs: none`), or a required template
/// input elsewhere on the card blocks it and sibling inputs ride along (F5).
fn execute(title: &str, token: &str, gather: bool) -> Value {
    let mut a = json!({
        "type": "Action.Execute", "verb": ACTION_VERB, "title": title,
        "data": { TOKEN_DATA_KEY: token },
    });
    if !gather {
        a["associatedInputs"] = json!("none");
    }
    a
}

impl Builder<'_, '_> {
    fn next_id(&mut self, prefix: &str) -> String {
        self.n += 1;
        format!("{prefix}-{}", self.n)
    }

    fn send_action(&mut self, title: &str, message: &str) -> Option<Value> {
        let token = (self.sign)(SEND_MESSAGE_TOOL, &text_args(message))?;
        Some(execute(title, &token, false))
    }

    /// A template submit: the one action its form asks for, in the host's primary style.
    fn template_action(&mut self, title: &str, template: &str) -> Option<Value> {
        let token = (self.sign)(SEND_MESSAGE_TOOL, &template_args(template))?;
        let mut a = execute(title, &token, true);
        a["style"] = json!("positive");
        Some(a)
    }

    /// The card action for a stateless [`Action`].
    fn action(&mut self, title: &str, a: &Action) -> Option<Value> {
        match a {
            Action::SendMessage { text } => self.send_action(title, text),
            Action::OpenUrl { url } => Some(json!({
                "type": "Action.OpenUrl", "title": title, "url": url,
            })),
        }
    }

    fn component(&mut self, c: &Component) -> Vec<Value> {
        match c {
            Component::List { title, items } => self.list(title.as_deref(), items),
            Component::Table {
                title,
                columns,
                rows,
            } => self.table(title.as_deref(), columns, rows),
            Component::Chips { prompt, chips } => self.chips(prompt.as_deref(), chips),
            Component::Tree {
                title,
                nodes,
                gaps_toggle,
            } => self.tree(title.as_deref(), nodes, gaps_toggle.as_deref()),
            Component::Notice { tone, title, text } => vec![notice(*tone, title.as_deref(), text)],
            Component::Progress {
                title,
                steps,
                fraction,
            } => progress(title.as_deref(), steps, *fraction),
            Component::KeyValue { title, pairs } => key_value(title.as_deref(), pairs),
            Component::Header {
                title,
                context,
                stamp,
            } => header(title, context.as_deref(), stamp.as_deref()),
            Component::FormGroups {
                title,
                groups,
                submit_label,
                note,
            } => self.form_groups(title.as_deref(), groups, submit_label, note.as_deref()),
            Component::Selection {
                prompt,
                options,
                args_key,
                style,
                submit: Some(sub),
                ..
            } => {
                // `args_key` is non-empty (Component::check), namespaced so a
                // second template component's same-named input can't collide.
                let ns = self.namespace();
                let id_owned = format!("{ns}{args_key}");
                let id = id_owned.as_str();
                let choices: Vec<Value> = options
                    .iter()
                    .map(|o| json!({ "title": o.label, "value": o.value }))
                    .collect();
                let style = match style {
                    triton_core::a2ui::SelectionStyle::Radio => "expanded",
                    triton_core::a2ui::SelectionStyle::Dropdown => "compact",
                };
                let mut out = vec![json!({
                    "type": "Input.ChoiceSet", "id": id, "label": prompt,
                    "style": style, "isRequired": true, "choices": choices,
                })];
                // The template names the input by `args_key`; the preview
                // shows the prompt where the choice goes.
                if let Some(label) = sub.preview_label.as_deref() {
                    let preview = preview_template(&sub.template, |n| {
                        (n == args_key.as_str()).then(|| prompt.clone())
                    });
                    out.push(preview_block(label, &preview));
                }
                let signed = rename_placeholders(&sub.template, |n| format!("{ns}{n}"));
                if let Some(a) = self.template_action(&sub.label, &signed) {
                    out.push(json!({ "type": "ActionSet", "actions": [a] }));
                }
                out
            }
            Component::Form {
                title,
                fields,
                submit_label,
                send_template: Some(tpl),
                preview_label,
                ..
            } => self.template_form(title, fields, submit_label, tpl, preview_label.as_deref()),
            Component::Embed {
                url,
                title,
                fallback,
                ..
            } => {
                // The fallback is the inline card (2a/2b) every host can show;
                // the button opens the embedded page in a task module where
                // the Teams app's `validDomains` allow the URL.
                let mut out = self.component(fallback);
                if triton_core::links::is_allowed_link(url)
                    && let Some(token) =
                        (self.sign)(OPEN_EMBED_TOOL, &json!({ "u": url, "t": title }))
                {
                    out.push(json!({
                        "type": "ActionSet",
                        "actions": [ {
                            "type": "Action.Submit", "title": title,
                            "data": { "msteams": { "type": "task/fetch" }, TOKEN_DATA_KEY: token },
                        } ],
                    }));
                }
                out
            }
            // Prose reaches here only as an embed's fallback (F3).
            Component::Text { value, .. } => {
                vec![json!({ "type": "TextBlock", "text": value, "wrap": true })]
            }
            Component::Narration { text } => vec![json!({
                "type": "TextBlock", "text": text, "wrap": true, "isSubtle": true,
            })],
            _ => Vec::new(),
        }
    }

    fn list(&mut self, title: Option<&str>, items: &[ListItem]) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(t) = title {
            out.push(heading(t));
        }
        for it in items {
            let mut inner = vec![json!({
                "type": "TextBlock", "text": it.label, "wrap": true,
                "color": if it.action.is_some() { "Accent" } else { "Default" },
            })];
            if let Some(d) = it.detail.as_deref().filter(|d| !d.is_empty()) {
                inner.push(json!({
                    "type": "TextBlock", "text": d, "isSubtle": true,
                    "spacing": "None", "size": "Small", "wrap": true,
                }));
            }
            let mut row = json!({ "type": "Container", "items": inner, "separator": true });
            if let Some(a) = it.action.as_ref().and_then(|a| self.action(&it.label, a)) {
                row["selectAction"] = a;
            }
            out.push(row);
        }
        out
    }

    /// A ColumnSet grid (AC 1.0+), not the 1.5 `Table`, so the card stays at
    /// the 1.4 floor; each actionable row is one `selectAction`.
    fn table(
        &mut self,
        title: Option<&str>,
        columns: &[TableColumn],
        rows: &[TableRow],
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(t) = title {
            out.push(heading(t));
        }
        let header: Vec<Value> = columns
            .iter()
            .map(|c| {
                json!({ "type": "Column", "width": "stretch", "items": [
                    { "type": "TextBlock", "text": c.label, "weight": "Bolder", "wrap": true,
                      "isSubtle": true, "size": "Small" }
                ] })
            })
            .collect();
        out.push(json!({ "type": "ColumnSet", "columns": header }));
        let first = columns.first().map(|c| c.key.clone()).unwrap_or_default();
        for r in rows {
            let cols: Vec<Value> = columns
                .iter()
                .map(|c| {
                    let v = r.cells.get(&c.key).cloned().unwrap_or_default();
                    json!({ "type": "Column", "width": "stretch", "items": [
                        { "type": "TextBlock", "text": v, "wrap": true }
                    ] })
                })
                .collect();
            let mut set = json!({ "type": "ColumnSet", "columns": cols, "separator": true });
            let label = r.cells.get(&first).cloned().unwrap_or_default();
            if let Some(a) = r.action.as_ref().and_then(|a| self.action(&label, a)) {
                set["selectAction"] = a;
            }
            out.push(set);
        }
        out
    }

    fn chips(&mut self, prompt: Option<&str>, chips: &[Chip]) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(p) = prompt {
            out.push(json!({ "type": "TextBlock", "text": p, "wrap": true }));
        }
        let actions: Vec<Value> = chips
            .iter()
            .filter_map(|c| self.send_action(&c.label, &c.message))
            .collect();
        if !actions.is_empty() {
            out.push(json!({ "type": "ActionSet", "actions": actions }));
        }
        out
    }

    fn tree(
        &mut self,
        title: Option<&str>,
        nodes: &[TreeNode],
        gaps_toggle: Option<&str>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(t) = title {
            out.push(heading(t));
        }
        let mut no_gap_ids: Vec<String> = Vec::new();
        let rendered: Vec<Value> = nodes
            .iter()
            .map(|n| self.tree_node(n, &mut no_gap_ids))
            .collect();
        // The toggle sits under the map, with the card's other actions.
        out.extend(rendered);
        if let Some(label) = gaps_toggle
            && !no_gap_ids.is_empty()
        {
            out.push(json!({
                "type": "ActionSet",
                "actions": [ {
                    "type": "Action.ToggleVisibility", "title": label,
                    "targetElements": no_gap_ids,
                } ],
            }));
        }
        out
    }

    /// A node: a header row (label + counts) whose tap toggles the
    /// children container, the leaf chips, the node's own action, and the
    /// children (indented by a Container).
    fn tree_node(&mut self, n: &TreeNode, no_gap_ids: &mut Vec<String>) -> Value {
        let node_id = self.next_id("node");
        if !n.has_gap() {
            no_gap_ids.push(node_id.clone());
        }
        let counts: Vec<String> = n
            .counts
            .iter()
            .map(|c| format!("{} {}", c.value, c.label))
            .collect();
        let has_inside = !n.children.is_empty() || !n.chips.is_empty();
        let children_id = self.next_id("kids");
        let expanded = n.expanded || n.gap;
        // Two chevrons toggled together with the children, so the marker
        // tracks the state client-side (▾ open, ▸ closed); a leaf gets a dot.
        let open_id = self.next_id("open");
        let closed_id = self.next_id("closed");
        let marker_items = if has_inside {
            json!([
                { "type": "TextBlock", "text": "\u{25be}", "id": open_id, "isVisible": expanded },
                { "type": "TextBlock", "text": "\u{25b8}", "id": closed_id, "isVisible": !expanded },
            ])
        } else {
            json!([ { "type": "TextBlock", "text": "\u{2022}" } ])
        };
        let mut header = json!({
            "type": "ColumnSet",
            "columns": [
                { "type": "Column", "width": "auto", "items": marker_items },
                { "type": "Column", "width": "stretch", "items": [
                    { "type": "TextBlock", "text": n.label, "wrap": true,
                      "weight": if n.gap { "Bolder" } else { "Default" },
                      "color": if n.gap { "Warning" } else { "Default" } } ] },
            ],
        });
        if !counts.is_empty() {
            header["columns"].as_array_mut().unwrap().push(json!({
                "type": "Column", "width": "auto", "items": [
                    { "type": "TextBlock", "text": counts.join(" \u{b7} "),
                      "isSubtle": true, "wrap": true } ] }));
        }
        if has_inside {
            header["selectAction"] = json!({
                "type": "Action.ToggleVisibility",
                "targetElements": [children_id, open_id, closed_id],
            });
        } else if let Some(a) = n
            .action
            .as_ref()
            .and_then(|a| self.action(n.action_caption(), a))
        {
            header["selectAction"] = a;
        }
        let mut items = vec![header];
        if has_inside {
            let mut inner: Vec<Value> = Vec::new();
            let chip_actions: Vec<Value> = n
                .chips
                .iter()
                .filter_map(|c| self.send_action(&c.label, &c.message))
                .collect();
            if !chip_actions.is_empty() {
                inner.push(json!({ "type": "ActionSet", "actions": chip_actions }));
            }
            if let Some(a) = n
                .action
                .as_ref()
                .and_then(|a| self.action(n.action_caption(), a))
            {
                inner.push(json!({ "type": "ActionSet", "actions": [a] }));
            }
            for c in &n.children {
                inner.push(self.tree_node(c, no_gap_ids));
            }
            items.push(json!({
                "type": "Container", "id": children_id, "isVisible": expanded,
                "spacing": "None",
                // Indent: an empty narrow column before the children.
                "items": [ { "type": "ColumnSet", "columns": [
                    { "type": "Column", "width": "12px", "items": [] },
                    { "type": "Column", "width": "stretch", "items": inner },
                ] } ],
            }));
        }
        json!({ "type": "Container", "id": node_id, "items": items, "spacing": "Small" })
    }

    fn template_form(
        &mut self,
        title: &str,
        fields: &[FormField],
        submit_label: &str,
        tpl: &str,
        preview_label: Option<&str>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if !title.is_empty() {
            out.push(heading(title));
        }
        let used = required_placeholders(tpl);
        let ns = self.namespace();
        for f in fields {
            out.push(input(f, &ns, used.iter().any(|u| u == &f.name)));
        }
        if let Some(plabel) = preview_label {
            let preview = preview_template(tpl, |name| {
                fields
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.label.clone())
            });
            out.push(preview_block(plabel, &preview));
        }
        let label = if submit_label.is_empty() {
            "Send"
        } else {
            submit_label
        };
        let signed = rename_placeholders(tpl, |n| format!("{ns}{n}"));
        if let Some(a) = self.template_action(label, &signed) {
            out.push(json!({ "type": "ActionSet", "actions": [a] }));
        }
        out
    }

    /// Form groups (3c): one `Action.ShowCard` per group, so the reader
    /// picks WHAT to change before seeing any field; each shown card holds
    /// the group's inputs and its own template submit (`Action.Execute`),
    /// then the note. Inputs are namespaced per group like any template
    /// component, and a submit gathers only its own card's inputs.
    fn form_groups(
        &mut self,
        title: Option<&str>,
        groups: &[FormGroup],
        submit_label: &str,
        note: Option<&str>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            out.push(heading(t));
        }
        let label = if submit_label.is_empty() {
            "Send"
        } else {
            submit_label
        };
        let mut openers = Vec::new();
        for g in groups {
            let ns = self.namespace();
            let used = required_placeholders(&g.send_template);
            let mut body: Vec<Value> = g
                .fields
                .iter()
                .map(|f| input(f, &ns, used.iter().any(|u| u == &f.name)))
                .collect();
            let signed = rename_placeholders(&g.send_template, |n| format!("{ns}{n}"));
            let Some(submit) = self.template_action(label, &signed) else {
                continue;
            };
            body.push(json!({ "type": "ActionSet", "actions": [submit] }));
            if let Some(n) = note.filter(|n| !n.is_empty()) {
                body.push(json!({
                    "type": "TextBlock", "text": n, "isSubtle": true, "size": "Small",
                    "wrap": true, "spacing": "Small",
                }));
            }
            openers.push(json!({
                "type": "Action.ShowCard", "title": g.label,
                "card": { "type": "AdaptiveCard", "body": body },
            }));
        }
        if !openers.is_empty() {
            out.push(json!({ "type": "ActionSet", "actions": openers }));
        }
        out
    }

    /// The input-id prefix of the next template component.
    fn namespace(&mut self) -> String {
        self.forms += 1;
        format!("t{}.", self.forms)
    }
}

/// "Will be sent as: …" — the complete request the submit will send, with
/// the user's choices marked. Shown only when the producer names it.
fn preview_block(label: &str, preview: &str) -> Value {
    json!({
        "type": "TextBlock", "text": format!("{label}: {preview}"),
        "isSubtle": true, "size": "Small", "wrap": true,
    })
}

/// One form input. A field the template fills is required: a message with
/// an unfilled placeholder is not a complete request.
fn input(f: &FormField, ns: &str, in_template: bool) -> Value {
    let required = f.required || in_template;
    let id = format!("{ns}{}", f.name);
    match f.kind {
        FormFieldKind::Select => {
            let choices: Vec<Value> = f
                .options
                .iter()
                .map(|o| json!({ "title": o.label, "value": o.value }))
                .collect();
            let mut v = json!({
                "type": "Input.ChoiceSet", "id": id, "label": f.label,
                "style": match f.style {
                    triton_core::a2ui::SelectionStyle::Radio => "expanded",
                    triton_core::a2ui::SelectionStyle::Dropdown => "compact",
                },
                "isRequired": required, "choices": choices,
            });
            if let Some(d) = f.default_value.as_ref().and_then(Value::as_str) {
                v["value"] = json!(d);
            }
            v
        }
        FormFieldKind::Text => text_input(f, &id, required, true),
        FormFieldKind::String => text_input(f, &id, required, false),
        FormFieldKind::Integer => json!({
            "type": "Input.Number", "id": id, "label": f.label, "isRequired": required,
        }),
        FormFieldKind::Boolean => json!({
            "type": "Input.Toggle", "id": id, "title": f.label,
        }),
    }
}

fn text_input(f: &FormField, id: &str, required: bool, multiline: bool) -> Value {
    let mut v = json!({
        "type": "Input.Text", "id": id, "label": f.label,
        "isRequired": required, "isMultiline": multiline,
    });
    if let Some(p) = &f.placeholder {
        v["placeholder"] = json!(p);
    }
    if let Some(d) = f.default_value.as_ref().and_then(Value::as_str) {
        v["value"] = json!(d);
    }
    v
}

/// The card head (one anatomy): the title in bold with the stamp (a time,
/// a timer) right-aligned beside it, then the one context line, subtle.
pub fn header(title: &str, context: Option<&str>, stamp: Option<&str>) -> Vec<Value> {
    let mut columns = vec![json!({ "type": "Column", "width": "stretch", "items": [
        { "type": "TextBlock", "text": title, "weight": "Bolder", "size": "Medium", "wrap": true }
    ] })];
    if let Some(s) = stamp.filter(|s| !s.is_empty()) {
        columns.push(json!({ "type": "Column", "width": "auto",
            "verticalContentAlignment": "Center", "items": [
            { "type": "TextBlock", "text": s, "isSubtle": true, "size": "Small" }
        ] }));
    }
    let mut out = vec![json!({ "type": "ColumnSet", "columns": columns })];
    if let Some(c) = context.filter(|c| !c.is_empty()) {
        out.push(json!({
            "type": "TextBlock", "text": c, "isSubtle": true, "wrap": true, "spacing": "None",
        }));
    }
    out
}

fn notice(tone: Tone, title: Option<&str>, text: &str) -> Value {
    let style = match tone {
        Tone::Info => "accent",
        Tone::Warn => "warning",
        Tone::Attention => "attention",
    };
    let mut items = Vec::new();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        items.push(heading(t));
    }
    items.push(json!({ "type": "TextBlock", "text": text, "wrap": true }));
    json!({ "type": "Container", "style": style, "items": items })
}

/// Status lines (AC has no progress bar): `✓` done, `▶` active (bold),
/// `○` pending, then the overall percentage.
fn progress(title: Option<&str>, steps: &[ProgressStep], fraction: Option<f64>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(t) = title {
        out.push(heading(t));
    }
    for s in steps {
        let (mark, weight, subtle) = match s.state {
            StepState::Done => ("\u{2713}", "Default", false),
            StepState::Active => ("\u{25b6}", "Bolder", false),
            StepState::Pending => ("\u{25cb}", "Default", true),
        };
        out.push(json!({
            "type": "TextBlock", "text": format!("{mark} {}", s.label),
            "weight": weight, "isSubtle": subtle, "spacing": "Small", "wrap": true,
        }));
    }
    if let Some(f) = fraction {
        out.push(json!({
            "type": "TextBlock",
            "text": format!("{}\u{a0}%", (f.clamp(0.0, 1.0) * 100.0).round() as u32),
            "isSubtle": true, "size": "Small",
        }));
    }
    out
}

fn key_value(title: Option<&str>, pairs: &[KeyValuePair]) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(t) = title {
        out.push(heading(t));
    }
    let facts: Vec<Value> = pairs
        .iter()
        .map(|p| {
            let v = match &p.before {
                Some(b) => format!("{b} \u{2192} {}", p.value),
                None => p.value.clone(),
            };
            json!({ "title": p.label, "value": v })
        })
        .collect();
    out.push(json!({ "type": "FactSet", "facts": facts }));
    out
}

/// The `task/continue` response that opens `url` in a Teams task module
/// (the host must be in the app manifest's `validDomains`).
pub fn url_task_response(title: &str, url: &str) -> Value {
    json!({
        "task": {
            "type": "continue",
            "value": {
                "title": title, "url": url, "fallbackUrl": url,
                "width": "large", "height": "large",
            },
        },
    })
}
