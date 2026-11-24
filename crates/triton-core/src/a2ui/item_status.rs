//! The status of a filed item on the MCP Apps answer card.
//!
//! A tool that files something for later (a change for review, a request
//! to a slower process) answers at once, and the outcome arrives minutes or
//! hours later. The card can show that outcome without the model: the
//! upstream names, in its result `_meta.item_status`, the read-only tool
//! that reads the item's status and its arguments, together with the
//! current state. Triton signs that call into an in-card token
//! ([`super::in_card::Scope::Status`]) and hands the card the state, the
//! token and a poll schedule in result `_meta["triton/status"]`. The card
//! calls the app-only tool with [`super::in_card::STATUS_EVENT`] and the
//! token, with growing intervals, until the status is terminal; each answer
//! carries the next state (and, while not terminal, a fresh token).
//!
//! Upstream shape (all other keys are ignored):
//!
//! ```json
//! { "item_status": {
//!     "tool": "request_status", "args": { "request_id": "r-1" },
//!     "state": "pending", "terminal": false, "label": "Waiting for review" } }
//! ```
//!
//! The status tool must be one the card may call in place (declared
//! `in_card: true` by the operator and annotated `readOnlyHint: true`);
//! otherwise the card shows the state without polling. A client that does
//! not render the card gets neither (its text is the upstream's own).

use serde_json::{Map, Value, json};

/// The upstream result `_meta` key Triton reads.
pub const UPSTREAM_KEY: &str = "item_status";
/// The result `_meta` key the card reads.
pub const META_KEY: &str = "triton/status";
/// Longest tool name taken.
pub const MAX_TOOL_CHARS: usize = 128;
/// Longest state taken (the upstream's own word: `pending`, `accepted`, ...).
pub const MAX_STATE_CHARS: usize = 64;
/// Longest label taken (what a person reads); longer ones are cut.
pub const MAX_LABEL_CHARS: usize = 200;
/// First poll after this long.
pub const POLL_AFTER_MS: u64 = 5_000;
/// Each interval is the last one times this, ...
pub const POLL_FACTOR: u64 = 2;
/// ... up to this.
pub const POLL_MAX_MS: u64 = 60_000;
/// The card stops polling this long after the answer arrived (it keeps the
/// last state shown).
pub const POLL_STOP_AFTER_MS: u64 = 30 * 60 * 1_000;

/// A filed item's status as the upstream reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemStatus {
    /// The read-only tool that reads the status again.
    pub tool: String,
    /// Its arguments (an object; `{}` when absent).
    pub args: Value,
    pub state: String,
    pub terminal: bool,
    pub label: Option<String>,
}

/// The item status in `result._meta.item_status`, `None` when absent or
/// malformed (a missing or over-long `tool` or `state`, a non-boolean
/// `terminal`, non-object `args`).
pub fn parse(result: &Value) -> Option<ItemStatus> {
    let s = result.get("_meta")?.get(UPSTREAM_KEY)?.as_object()?;
    let tool = s.get("tool")?.as_str()?.trim();
    let tool_ok = !tool.is_empty()
        && tool.chars().count() <= MAX_TOOL_CHARS
        && tool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    let state = s.get("state")?.as_str()?.trim();
    let state_ok = !state.is_empty()
        && state.chars().count() <= MAX_STATE_CHARS
        && !state.chars().any(char::is_control);
    let terminal = match s.get("terminal") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return None,
    };
    let args = match s.get("args") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(a) if a.is_object() => a.clone(),
        Some(_) => return None,
    };
    if !tool_ok || !state_ok {
        return None;
    }
    let label = s
        .get("label")
        .and_then(Value::as_str)
        .map(clean_label)
        .filter(|l| !l.is_empty());
    Some(ItemStatus {
        tool: tool.to_string(),
        args,
        state: state.to_string(),
        terminal,
        label,
    })
}

/// One line of at most [`MAX_LABEL_CHARS`]: control characters become
/// spaces, runs of whitespace one space.
fn clean_label(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_LABEL_CHARS)
        .collect()
}

/// The card's `_meta["triton/status"]`: the state, and with `token` (a
/// non-terminal status the card may poll) the token and the schedule.
pub fn meta(status: &ItemStatus, token: Option<String>) -> Value {
    let mut out = json!({ "state": status.state, "terminal": status.terminal });
    if let Some(l) = &status.label {
        out["label"] = json!(l);
    }
    if let Some(t) = token.filter(|_| !status.terminal) {
        out["token"] = json!(t);
        out["poll"] = json!({
            "after_ms": POLL_AFTER_MS,
            "factor": POLL_FACTOR,
            "max_ms": POLL_MAX_MS,
            "stop_after_ms": POLL_STOP_AFTER_MS,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(status: Value) -> Value {
        json!({ "text": "Filed.", "_meta": { UPSTREAM_KEY: status } })
    }

    #[test]
    fn a_status_names_its_tool_args_state_and_label() {
        let s = parse(&with(json!({
            "tool": "request_status", "args": { "request_id": "r-1" },
            "state": "pending", "terminal": false, "label": "Waiting\nfor  review",
            "other": 1
        })))
        .unwrap();
        assert_eq!(
            s,
            ItemStatus {
                tool: "request_status".into(),
                args: json!({ "request_id": "r-1" }),
                state: "pending".into(),
                terminal: false,
                label: Some("Waiting for review".into()),
            }
        );
    }

    #[test]
    fn terminal_and_args_and_label_are_optional() {
        let s = parse(&with(json!({ "tool": "t", "state": "accepted" }))).unwrap();
        assert!(!s.terminal);
        assert_eq!(s.args, json!({}));
        assert_eq!(s.label, None);
        let s = parse(&with(
            json!({ "tool": "t", "state": "done", "terminal": true,
                                     "label": "  " }),
        ))
        .unwrap();
        assert!(s.terminal);
        assert_eq!(s.label, None);
    }

    #[test]
    fn a_malformed_status_is_none() {
        for bad in [
            json!("pending"),
            json!({ "state": "pending" }),
            json!({ "tool": "", "state": "pending" }),
            json!({ "tool": "a b", "state": "pending" }),
            json!({ "tool": "x".repeat(MAX_TOOL_CHARS + 1), "state": "pending" }),
            json!({ "tool": "t" }),
            json!({ "tool": "t", "state": "" }),
            json!({ "tool": "t", "state": "s".repeat(MAX_STATE_CHARS + 1) }),
            json!({ "tool": "t", "state": "a\nb" }),
            json!({ "tool": "t", "state": "pending", "terminal": "yes" }),
            json!({ "tool": "t", "state": "pending", "args": [1] }),
        ] {
            assert_eq!(parse(&with(bad.clone())), None, "{bad}");
        }
        assert_eq!(parse(&json!({ "text": "no meta" })), None);
    }

    #[test]
    fn a_long_label_is_cut() {
        let s = parse(&with(json!({ "tool": "t", "state": "s",
                                     "label": "é".repeat(MAX_LABEL_CHARS + 5) })))
        .unwrap();
        assert_eq!(s.label.unwrap().chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn only_a_pending_status_with_a_token_is_polled() {
        let pending = parse(&with(json!({ "tool": "t", "state": "pending",
                                           "label": "Waiting" })))
        .unwrap();
        let m = meta(&pending, Some("tok".into()));
        assert_eq!(m["state"], "pending");
        assert_eq!(m["terminal"], false);
        assert_eq!(m["label"], "Waiting");
        assert_eq!(m["token"], "tok");
        assert_eq!(m["poll"]["after_ms"], POLL_AFTER_MS);
        assert_eq!(m["poll"]["max_ms"], POLL_MAX_MS);

        let unpolled = meta(&pending, None);
        assert!(unpolled.get("token").is_none() && unpolled.get("poll").is_none());

        let done = ItemStatus {
            terminal: true,
            ..pending
        };
        let m = meta(&done, Some("tok".into()));
        assert_eq!(m["terminal"], true);
        assert!(m.get("token").is_none() && m.get("poll").is_none(), "{m}");
    }
}
