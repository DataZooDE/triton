//! Stateless "send this message" actions (conversation components).
//!
//! A chip, a row action, a Selection submit or a Form submit in a channel
//! surface does not re-invoke a tool with partial arguments, and it does not
//! continue a multi-turn dialogue. It sends ONE complete new user message —
//! exactly the text the user could have typed — and that message becomes a
//! new turn. Everything a click needs therefore travels inside the action's
//! signed payload, and no process has to remember the card:
//!
//! * a fixed message rides as `(SEND_MESSAGE_TOOL, {"m": text})`;
//! * a message completed from the user's input (Selection, Form) rides as
//!   `(SEND_MESSAGE_TOOL, {"tpl": template})`, and the channel's callback
//!   fills the template's `{name}` placeholders from the gathered inputs.
//!
//! The `(tool, args)` pair is signed with the ordinary correlation token
//! ([`crate::encode_bound`]), so a channel that already verifies card tokens
//! needs no second mechanism. The inputs are NOT signed: they are what the
//! user chose or typed, and the message they complete is no more privileged
//! than one the user could have typed directly.

use serde_json::{Value, json};

/// Sentinel tool name for a send-message action. Never dispatched as a tool:
/// a channel that decodes it turns the payload into the text of a new
/// inbound user message and routes that text the way it routes typed input.
/// The leading underscores keep it out of any real tool namespace (Triton
/// tool names are agent-chosen identifiers).
pub const SEND_MESSAGE_TOOL: &str = "__send_message";

/// The args a send-message token carries for a fixed message.
pub fn text_args(text: &str) -> Value {
    json!({ "m": text })
}

/// The args a send-message token carries for a message completed from the
/// user's input on submit.
pub fn template_args(template: &str) -> Value {
    json!({ "tpl": template })
}

/// Why a send-message payload could not be turned into a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    /// Neither `m` nor `tpl` is present (or either is not a string).
    #[error("send-message payload carries neither a message nor a template")]
    Empty,
    /// The template names inputs the submit did not carry. A message with a
    /// literal `{capability}` in it is not a complete request, so it is not
    /// sent; the caller tells the user which choice is missing.
    #[error("missing input(s): {}", .0.join(", "))]
    Missing(Vec<String>),
}

/// The placeholder names in `template`, in order of first appearance.
///
/// A placeholder is `{name}` with `name` drawn from `[A-Za-z0-9_.-]` and
/// non-empty. Anything else in braces (`{ }`, `{a b}`, a lone `{`) is literal
/// text, so a template can carry JSON-ish or set-notation text without an
/// escaping scheme.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _, _) in scan(template) {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Fill every `{name}` placeholder with `value(name)`; placeholders the
/// closure has no value for are reported, not left in the text.
///
/// An optional segment `[[ … ]]` (e.g. `[[ Reason: {reason}]]`) is kept —
/// without its brackets — only when every placeholder inside it has a
/// value, and dropped whole otherwise; its placeholders are never reported
/// missing. Segments do not nest.
pub fn fill_template(
    template: &str,
    value: impl Fn(&str) -> Option<String>,
) -> Result<String, MessageError> {
    let mut out = String::with_capacity(template.len());
    let mut missing: Vec<String> = Vec::new();
    for (part, optional) in segments(template) {
        let mut part_missing: Vec<String> = Vec::new();
        let filled = fill_part(part, &value, &mut part_missing);
        if optional {
            if part_missing.is_empty() {
                out.push_str(&filled);
            }
        } else {
            out.push_str(&filled);
            for m in part_missing {
                if !missing.contains(&m) {
                    missing.push(m);
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(MessageError::Missing(missing))
    }
}

/// The placeholders a complete request cannot do without: those outside
/// every optional `[[ … ]]` segment.
pub fn required_placeholders(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (part, optional) in segments(template) {
        if optional {
            continue;
        }
        for (name, _, _) in scan(part) {
            if !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn fill_part(
    part: &str,
    value: &impl Fn(&str) -> Option<String>,
    missing: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(part.len());
    let mut last = 0usize;
    for (name, start, end) in scan(part) {
        out.push_str(&part[last..start]);
        match value(name).filter(|v| !v.trim().is_empty()) {
            Some(v) => out.push_str(v.trim()),
            None => {
                if !missing.iter().any(|m| m == name) {
                    missing.push(name.to_string());
                }
            }
        }
        last = end;
    }
    out.push_str(&part[last..]);
    out
}

/// Split into `(text, optional)` parts at `[[` / `]]`. An unclosed `[[` is
/// literal text.
fn segments(template: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("[[") {
        let Some(close) = rest[open + 2..].find("]]") else {
            break;
        };
        if open > 0 {
            out.push((&rest[..open], false));
        }
        out.push((&rest[open + 2..open + 2 + close], true));
        rest = &rest[open + 2 + close + 2..];
    }
    if !rest.is_empty() {
        out.push((rest, false));
    }
    out
}

/// Render a template for DISPLAY ("will be sent as …"): each placeholder is
/// replaced by `label(name)` in angle quotes, so the reader sees which part
/// their choice fills. Unknown names show the bare name.
pub fn preview_template(template: &str, label: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    // Optional segments show without their brackets.
    for (part, _) in segments(template) {
        let mut last = 0usize;
        for (name, start, end) in scan(part) {
            out.push_str(&part[last..start]);
            out.push('\u{2039}');
            out.push_str(&label(name).unwrap_or_else(|| name.to_string()));
            out.push('\u{203a}');
            last = end;
        }
        out.push_str(&part[last..]);
    }
    out
}

/// Rename every placeholder (`{name}` → `{rename(name)}`), leaving the
/// rest of the template untouched. Teams uses it to namespace a
/// component's input ids on a card where ids are global.
pub fn rename_placeholders(template: &str, rename: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(template.len() + 8);
    let mut last = 0usize;
    for (name, start, end) in scan(template) {
        out.push_str(&template[last..start]);
        out.push('{');
        out.push_str(&rename(name));
        out.push('}');
        last = end;
    }
    out.push_str(&template[last..]);
    out
}

/// Turn a verified send-message payload plus the user's submitted inputs
/// into the complete message text.
pub fn resolve(
    args: &Value,
    input: impl Fn(&str) -> Option<String>,
) -> Result<String, MessageError> {
    if let Some(m) = args.get("m").and_then(Value::as_str) {
        let m = m.trim();
        return if m.is_empty() {
            Err(MessageError::Empty)
        } else {
            Ok(m.to_string())
        };
    }
    if let Some(tpl) = args.get("tpl").and_then(Value::as_str) {
        let text = fill_template(tpl, input)?;
        return if text.trim().is_empty() {
            Err(MessageError::Empty)
        } else {
            Ok(text.trim().to_string())
        };
    }
    Err(MessageError::Empty)
}

/// `(name, byte_start, byte_end)` for each well-formed placeholder.
fn scan(template: &str) -> Vec<(&str, usize, usize)> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut j = i + 1;
            while j < bytes.len() && is_name_byte(bytes[j]) {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b'}' {
                out.push((&template[i + 1..j], i, j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn inputs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_fixed_message_resolves_to_itself() {
        let args = text_args("Show agents in Procurement without an owner");
        let none: BTreeMap<String, String> = BTreeMap::new();
        assert_eq!(
            resolve(&args, |k| none.get(k).cloned()).unwrap(),
            "Show agents in Procurement without an owner"
        );
    }

    #[test]
    fn a_template_is_filled_from_the_submitted_inputs() {
        let args = template_args("Show agents in {capability} owned by {owner}");
        let got = inputs(&[("capability", "Procurement"), ("owner", "Anna Weber")]);
        assert_eq!(
            resolve(&args, |k| got.get(k).cloned()).unwrap(),
            "Show agents in Procurement owned by Anna Weber"
        );
    }

    /// A message with a literal `{capability}` is not a complete request —
    /// it is refused with the missing names, never sent half-filled.
    #[test]
    fn a_missing_or_blank_input_is_reported_not_sent() {
        let args = template_args("Show agents in {capability} owned by {owner}");
        let got = inputs(&[("capability", "Procurement"), ("owner", "  ")]);
        assert_eq!(
            resolve(&args, |k| got.get(k).cloned()),
            Err(MessageError::Missing(vec!["owner".into()]))
        );
    }

    #[test]
    fn non_placeholder_braces_are_literal_text() {
        assert_eq!(placeholders("set {a b} or { } or {x} or {x}"), vec!["x"]);
        let got = inputs(&[("x", "1")]);
        assert_eq!(
            fill_template("set {a b} or {x}", |k| got.get(k).cloned()).unwrap(),
            "set {a b} or 1"
        );
    }

    /// 1e/1o "Reason (optional)": a `[[…]]` segment is kept (without the
    /// brackets) only when every placeholder inside it has a value, else
    /// dropped whole — no dangling "Reason: ." in the sent request.
    #[test]
    fn an_optional_segment_is_kept_only_when_filled() {
        let tpl = "Retire the agent \"{agent}\".[[ Reason: {reason}]]";
        let with = inputs(&[("agent", "Agent 20"), ("reason", "replaced by Agent 31")]);
        assert_eq!(
            fill_template(tpl, |k| with.get(k).cloned()).unwrap(),
            "Retire the agent \"Agent 20\". Reason: replaced by Agent 31"
        );
        let without = inputs(&[("agent", "Agent 20"), ("reason", " ")]);
        assert_eq!(
            fill_template(tpl, |k| without.get(k).cloned()).unwrap(),
            "Retire the agent \"Agent 20\"."
        );
        // A required placeholder outside the segment is still required.
        assert_eq!(
            fill_template(tpl, |_| None),
            Err(MessageError::Missing(vec!["agent".into()]))
        );
        assert_eq!(required_placeholders(tpl), vec!["agent"]);
        assert_eq!(placeholders(tpl), vec!["agent", "reason"]);
        assert_eq!(
            preview_template(tpl, |n| Some(n.to_uppercase())),
            "Retire the agent \"\u{2039}AGENT\u{203a}\". Reason: \u{2039}REASON\u{203a}"
        );
        assert_eq!(
            rename_placeholders(tpl, |n| format!("t1.{n}")),
            "Retire the agent \"{t1.agent}\".[[ Reason: {t1.reason}]]"
        );
    }

    #[test]
    fn preview_marks_each_placeholder_with_its_label() {
        let p = preview_template("Show agents in {capability}", |n| {
            (n == "capability").then(|| "Capability".to_string())
        });
        assert_eq!(p, "Show agents in \u{2039}Capability\u{203a}");
    }

    #[test]
    fn an_empty_payload_is_an_error() {
        assert_eq!(resolve(&json!({}), |_| None), Err(MessageError::Empty));
        assert_eq!(
            resolve(&text_args("  "), |_| None),
            Err(MessageError::Empty)
        );
    }

    /// The full stateless round trip: the payload is signed with an ordinary
    /// bound correlation token, so any replica holding the key can recover
    /// the complete message from the token alone.
    #[test]
    fn a_signed_send_message_token_round_trips_on_any_replica() {
        let key = b"correlation-key-for-test";
        let b = crate::Binding {
            platform: "msteams",
            tenant: "acme",
            sender: "29:1abc",
        };
        let token = crate::encode_bound(
            SEND_MESSAGE_TOOL,
            &template_args("Open {view}"),
            key,
            1536,
            b,
            Some(3600),
        )
        .unwrap();
        let (tool, args) = crate::decode_bound(&token, key, 1536, b).unwrap();
        assert_eq!(tool, SEND_MESSAGE_TOOL);
        let got = inputs(&[("view", "the capability map")]);
        assert_eq!(
            resolve(&args, |k| got.get(k).cloned()).unwrap(),
            "Open the capability map"
        );
    }
}
