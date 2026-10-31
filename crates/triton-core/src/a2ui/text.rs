//! Plain-text degradation of the conversation components, for every channel
//! with no native widget for them (Telegram, Signal, WhatsApp, SMS, email,
//! the Gemini answer surface, MCP text content).
//!
//! The rules follow the one product rule that matters on a text channel:
//! **no multi-turn.** A text reader cannot click, and a reply like "2" would
//! arrive as a new message with no context. So every action that would send a
//! message is shown as the complete line to copy and send (`› …`), and every
//! deep link is shown as its URL. Triton adds no natural-language words of
//! its own (the producer writes EN or DE); the only additions are layout
//! marks: `›` for a line to send, `✓ ▶ ○` for step states, `→` for a change,
//! `ⓘ`/`⚠` for a notice.
//!
//! Version-agnostic, like [`super::diff_to_text`]: it works on the canonical
//! model, not on a wire shape.

use super::{
    Action, Chip, Component, FormFieldKind, KeyValuePair, ListItem, ProgressStep, StepState,
    TableColumn, TableRow, Tone, TreeNode,
};

/// Mark for a line the reader can copy and send as a new message.
pub const SEND_MARK: &str = "\u{203a} ";

/// The text degradation of `c`, or `None` for a component that has no
/// conversation-component degradation here (the legacy kinds keep their
/// per-channel rules).
pub fn degrade(c: &Component) -> Option<String> {
    let out = match c {
        Component::List { title, items } => list(title.as_deref(), items),
        Component::Table {
            title,
            columns,
            rows,
        } => table(title.as_deref(), columns, rows),
        Component::Chips { prompt, chips } => chips_text(prompt.as_deref(), chips),
        Component::Tree { title, nodes, .. } => tree(title.as_deref(), nodes),
        Component::Notice { tone, title, text } => notice(*tone, title.as_deref(), text),
        Component::Progress {
            title,
            steps,
            fraction,
        } => progress(title.as_deref(), steps, *fraction),
        Component::KeyValue { title, pairs } => key_value(title.as_deref(), pairs),
        Component::Embed {
            url,
            title,
            fallback,
            ..
        } => {
            // Any inline component, prose included (F3).
            let mut s = surface_to_text(&super::Surface {
                components: vec![(**fallback).clone()],
            });
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(&format!("{title}: {url}"));
            s
        }
        Component::Button {
            label,
            action: Some(a),
            ..
        } => action_line(label, a),
        Component::Selection {
            prompt,
            options,
            args_key,
            submit: Some(sub),
            ..
        } => {
            let mut lines = vec![prompt.clone()];
            // An option whose template does not fill completely is not a
            // complete request: it is skipped, as on Gemini (F9).
            for o in options {
                if let Ok(msg) = triton_correlation::message::fill_template(&sub.template, |k| {
                    (k == args_key).then(|| o.value.clone())
                }) {
                    lines.push(format!("{SEND_MARK}{msg}"));
                }
            }
            lines.retain(|l| !l.trim().is_empty());
            lines.join("\n")
        }
        Component::Form {
            title,
            fields,
            send_template: Some(tpl),
            ..
        } => {
            let mut lines: Vec<String> = Vec::new();
            if !title.is_empty() {
                lines.push(title.clone());
            }
            let preview = triton_correlation::message::preview_template(tpl, |name| {
                fields
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.label.clone())
            });
            lines.push(format!("{SEND_MARK}{preview}"));
            for f in fields {
                if matches!(f.kind, FormFieldKind::Select) && !f.options.is_empty() {
                    let opts: Vec<&str> = f.options.iter().map(|o| o.label.as_str()).collect();
                    lines.push(format!("  {}: {}", f.label, opts.join(" | ")));
                }
            }
            lines.join("\n")
        }
        _ => return None,
    };
    let out = out.trim_end().to_string();
    (!out.is_empty()).then_some(out)
}

/// A button or row action as one line: the URL to open, or the message to
/// send.
pub fn action_line(label: &str, a: &Action) -> String {
    match a {
        Action::OpenUrl { url } => format!("{label}: {url}"),
        Action::SendMessage { text } => format!("{SEND_MARK}{text}"),
    }
}

fn heading(out: &mut Vec<String>, title: Option<&str>) {
    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        out.push(t.to_string());
    }
}

fn list(title: Option<&str>, items: &[ListItem]) -> String {
    let mut out = Vec::new();
    heading(&mut out, title);
    for (i, it) in items.iter().enumerate() {
        let mut line = format!("{}. {}", i + 1, it.label);
        if let Some(d) = it.detail.as_deref().filter(|d| !d.is_empty()) {
            line.push_str(&format!(" \u{2014} {d}"));
        }
        out.push(line);
        match &it.action {
            Some(Action::OpenUrl { url }) => out.push(format!("   {url}")),
            Some(Action::SendMessage { text }) => out.push(format!("   {SEND_MARK}{text}")),
            None => {}
        }
    }
    out.join("\n")
}

fn table(title: Option<&str>, columns: &[TableColumn], rows: &[TableRow]) -> String {
    let mut out = Vec::new();
    heading(&mut out, title);
    for (i, r) in rows.iter().enumerate() {
        let cells: Vec<String> = columns
            .iter()
            .filter_map(|c| {
                r.cells
                    .get(&c.key)
                    .filter(|v| !v.is_empty())
                    .map(|v| (c, v))
            })
            .enumerate()
            .map(|(j, (c, v))| {
                if j == 0 {
                    v.clone()
                } else {
                    format!("{}: {v}", c.label)
                }
            })
            .collect();
        out.push(format!("{}. {}", i + 1, cells.join(" \u{b7} ")));
        match &r.action {
            Some(Action::OpenUrl { url }) => out.push(format!("   {url}")),
            Some(Action::SendMessage { text }) => out.push(format!("   {SEND_MARK}{text}")),
            None => {}
        }
    }
    out.join("\n")
}

fn chips_text(prompt: Option<&str>, chips: &[Chip]) -> String {
    let mut out = Vec::new();
    heading(&mut out, prompt);
    for c in chips {
        out.push(format!("{SEND_MARK}{}", c.message));
    }
    out.join("\n")
}

fn tree(title: Option<&str>, nodes: &[TreeNode]) -> String {
    let mut out = Vec::new();
    heading(&mut out, title);
    for n in nodes {
        tree_node(&mut out, n, 0);
    }
    out.join("\n")
}

fn counts_suffix(n: &TreeNode) -> String {
    if n.counts.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = n
        .counts
        .iter()
        .map(|c| format!("{} {}", c.value, c.label))
        .collect();
    format!(" \u{2014} {}", parts.join(" \u{b7} "))
}

/// A leaf: no children. A leaf that is a gap (no chips, nothing inside)
/// is collapsed with its gap siblings into one line (2c: "the gaps stay
/// visible without noise").
fn is_gap_leaf(n: &TreeNode) -> bool {
    n.gap && n.children.is_empty() && n.chips.is_empty()
}

fn tree_node(out: &mut Vec<String>, n: &TreeNode, depth: usize) {
    let indent = "  ".repeat(depth);
    let gap = if n.gap { " \u{26a0}" } else { "" };
    let inner = "  ".repeat(depth + 1);
    if n.children.is_empty() && !n.chips.is_empty() {
        // A leaf with its Agents on one line: `Forecast: Agent 28 · Planned`.
        let labels: Vec<&str> = n.chips.iter().map(|c| c.label.as_str()).collect();
        out.push(format!(
            "{indent}{}{}{gap}: {}",
            n.label,
            counts_suffix(n),
            labels.join(", ")
        ));
    } else {
        out.push(format!("{indent}{}{}{gap}", n.label, counts_suffix(n)));
        if !n.chips.is_empty() {
            let labels: Vec<&str> = n.chips.iter().map(|c| c.label.as_str()).collect();
            out.push(format!("{inner}\u{b7} {}", labels.join(", ")));
        }
    }
    match &n.action {
        Some(Action::OpenUrl { url }) => out.push(format!("{inner}{}: {url}", n.action_caption())),
        Some(Action::SendMessage { text }) => out.push(format!("{inner}{SEND_MARK}{text}")),
        None => {}
    }
    // Non-gap children first, then the gap leaves on one line. A gap leaf's
    // own action is a per-capability follow-up the reader can type; listing
    // one copy line per empty capability is the noise 2c avoids.
    for c in n.children.iter().filter(|c| !is_gap_leaf(c)) {
        tree_node(out, c, depth + 1);
    }
    let gaps: Vec<&str> = n
        .children
        .iter()
        .filter(|c| is_gap_leaf(c))
        .map(|c| c.label.as_str())
        .collect();
    if !gaps.is_empty() {
        out.push(format!("{inner}{} \u{26a0}", gaps.join(", ")));
    }
}

fn notice(tone: Tone, title: Option<&str>, text: &str) -> String {
    let mark = match tone {
        Tone::Info => "\u{24d8}",
        Tone::Warn => "\u{26a0}",
    };
    match title.filter(|t| !t.is_empty()) {
        Some(t) => format!("{mark} {t}: {text}"),
        None => format!("{mark} {text}"),
    }
}

fn progress(title: Option<&str>, steps: &[ProgressStep], fraction: Option<f64>) -> String {
    let mut out = Vec::new();
    heading(&mut out, title);
    for s in steps {
        let mark = match s.state {
            StepState::Done => "\u{2713}",
            StepState::Active => "\u{25b6}",
            StepState::Pending => "\u{25cb}",
        };
        out.push(format!("{mark} {}", s.label));
    }
    if let Some(f) = fraction {
        let f = f.clamp(0.0, 1.0);
        let filled = (f * 10.0).round() as usize;
        out.push(format!(
            "[{}{}] {}\u{a0}%",
            "#".repeat(filled),
            "-".repeat(10 - filled),
            (f * 100.0).round() as u32
        ));
    }
    out.join("\n")
}

fn key_value(title: Option<&str>, pairs: &[KeyValuePair]) -> String {
    let mut out = Vec::new();
    heading(&mut out, title);
    for p in pairs {
        match &p.before {
            Some(b) => out.push(format!("{}: {b} \u{2192} {}", p.label, p.value)),
            None => out.push(format!("{}: {}", p.label, p.value)),
        }
    }
    out.join("\n")
}

/// A whole surface as plain text — the reference degradation a text-only
/// channel (or an MCP `text` content block) sends: prose as-is, legacy
/// read-only kinds in their simplest form, conversation components via
/// [`degrade`]. Legacy tool controls (a `Button` without an action, a
/// Selection/Form without a template) have no text form and are skipped:
/// they can only be answered by re-invoking a tool, which a text reader
/// cannot do.
pub fn surface_to_text(surface: &super::Surface) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for c in &surface.components {
        match c {
            Component::Text { value, .. } => chunks.push(value.clone()),
            Component::Narration { text } => chunks.push(text.clone()),
            Component::Diff { lines } => chunks.push(super::diff_to_text(lines)),
            Component::Dashboard { title, tiles } => {
                let mut out = vec![title.clone()];
                out.extend(tiles.iter().map(|t| format!("{}: {}", t.label, t.value)));
                chunks.push(out.join("\n"));
            }
            Component::Sources { items } => chunks.push(
                items
                    .iter()
                    .map(|i| {
                        if crate::links::is_allowed_link(&i.resource) {
                            format!("{}: {}", i.label, i.resource)
                        } else {
                            i.label.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            other => chunks.extend(degrade(other)),
        }
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks.join("\n\n")
}
