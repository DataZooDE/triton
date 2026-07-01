//! v0.2 PR 35 — L6' surface mapper for the Microsoft Teams adapter.
//!
//! Bot Framework Activity replies are text-first; the connector's
//! published body cap is ~28 KB but we use Telegram's safer 4096-byte
//! ceiling so a single oversized tool output can never trip the
//! upstream length check. The current scope ships the text path
//! (Text + Narration); richer Teams surfaces (AdaptiveCard) wait for
//! the L6' AdaptiveCard pass referenced in architecture.md §8.7.
//!
//! Buttons / Selections / Forms / Dashboards are counted as deferred
//! so the operator sees the gap in tracing without ever leaking the
//! component contents into the Activity body.
//!
//! No HTML escaping: Teams renders Activity `text` as Markdown by
//! default, but for the conservative slice this PR ships we keep
//! `textFormat: plain` and pass the raw string through.

use serde_json::{Value, json};
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Hard ceiling on the Activity `text` body. The Bot Framework
/// documented limit is ~28 KB; we keep Telegram's 4096-byte safe
/// ceiling so chat-channel adapters share one truncation budget and
/// operator dashboards stay comparable.
pub const MSTEAMS_TEXT_MAX_BYTES: usize = 4096;

/// Sentinel appended when we truncate to fit
/// [`MSTEAMS_TEXT_MAX_BYTES`]. Square brackets make it visibly an
/// adapter artefact, not tool output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded the 4096-byte Activity cap]";

/// Rendered Teams Activity body. PR 35 only carries `text`; the
/// future AdaptiveCard pass will add `attachments` here.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

/// Mapper-edge failure modes the caller MUST handle without sending
/// a Teams Activity. Mirrors the Telegram + Discord mappers so the
/// outbound courier never receives an empty payload.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    EmptyAfterRender,
}

/// Try to render `result` as a Teams Activity body. `None` when the
/// result isn't an A2UI surface so callers can fall through to the
/// bare-text path.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a `RenderedMessage` or a `RenderError`.
/// Public so in-crate unit tests can exercise the mapper without
/// spinning the whole binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    for c in &surface.components {
        match c {
            Component::Text { value } => chunks.push(value.clone()),
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored by the text mapper.
            Component::Report { .. } => {}
            Component::Narration { text } => chunks.push(format!("_{}_", text)),
            Component::Button { .. } => deferred_buttons += 1,
            Component::Selection { .. } => deferred_selections += 1,
            Component::Form { .. } => deferred_forms += 1,
            Component::Dashboard { .. } => deferred_dashboards += 1,
        }
    }
    if chunks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    let joined = chunks.join("\n\n");
    if joined.len() <= MSTEAMS_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
        });
    }

    // Walk the chunk prefix that fits under `cap - sentinel`. Every
    // accepted chunk lands on a component boundary (no inside-cuts),
    // so the rendered body is always a syntactically intact prefix.
    let budget = MSTEAMS_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in &chunks {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 };
        if total + sep_cost + chunk.len() > budget {
            break;
        }
        total += sep_cost + chunk.len();
        accepted.push(chunk.as_str());
    }
    if !accepted.is_empty() {
        let mut out = accepted.join("\n\n");
        out.push_str(TRUNCATION_SENTINEL);
        return Ok(RenderedMessage {
            text: out,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: true,
        });
    }

    // Even the first chunk overflows: truncate at a char boundary so
    // a multi-byte UTF-8 sequence is never split. `floor_char_boundary`
    // is nightly-only; emulate by walking down to the previous
    // boundary by hand.
    let first = &chunks[0];
    let target = budget.min(first.len());
    let mut cut = target;
    while cut > 0 && !first.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = first[..cut].to_string();
    out.push_str(TRUNCATION_SENTINEL);
    Ok(RenderedMessage {
        text: out,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        deferred_dashboards,
        truncated: true,
    })
}

/// Build a Bot Framework Activity reply body for the conversation
/// the inbound message arrived on. `recipient` mirrors the inbound's
/// `from.id` (the user we're replying to) and `from` mirrors the
/// inbound's `recipient.id` (the bot itself), matching the
/// Microsoft Bot Connector documented shape for reply activities.
pub fn build_activity_body(
    bot_id: &str,
    conversation_id: &str,
    recipient_id: &str,
    msg: &RenderedMessage,
) -> Value {
    json!({
        "type": "message",
        "from": { "id": bot_id },
        "conversation": { "id": conversation_id },
        "recipient": { "id": recipient_id },
        "text": msg.text,
        "textFormat": "plain",
    })
}

/// Clamp a plain-text fallback (non-A2UI tool result) to the same
/// 4096-byte ceiling. UTF-8 boundary safe. Operators get the same
/// guarantee whether the tool returned a Surface or a bare string.
pub fn clamp_plain_text(s: &str) -> String {
    if s.len() <= MSTEAMS_TEXT_MAX_BYTES {
        return s.to_string();
    }
    let budget = MSTEAMS_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut cut = budget.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str(TRUNCATION_SENTINEL);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::{Component, Surface};

    #[test]
    fn passthrough_text_and_narration() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "hello".into(),
                },
                Component::Narration {
                    text: "a footnote".into(),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "hello\n\n_a footnote_");
        assert!(!r.truncated);
        assert_eq!(r.deferred_buttons, 0);
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(10_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= MSTEAMS_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn buttons_are_counted_as_deferred() {
        let s = Surface {
            components: vec![
                Component::Text { value: "x".into() },
                Component::Button {
                    label: "Click".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.deferred_buttons, 1);
    }
}
