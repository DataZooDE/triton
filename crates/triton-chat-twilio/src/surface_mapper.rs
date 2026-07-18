//! L6' surface mapper for Twilio-WhatsApp (PR-T2 scope: Text + Narration
//! only — mirrors WhatsApp Cloud's own PR 31 scope). Buttons / Selection /
//! Form / Dashboard are counted into `deferred_*` and logged, not
//! rendered; interactive primitives land in a follow-up PR once this
//! text-only path is proven, exactly as WhatsApp Cloud's PR 32 followed
//! its PR 31.
//!
//! Each chat adapter owns its own L6' rendering concerns (mirrors the
//! WhatsApp/Telegram/Discord mappers) — sharing the type across crates
//! would couple unrelated platforms' wire shapes together.

use triton_core::a2ui::{Component, Surface, extract_surface};

/// Twilio's documented single-message body limit (WhatsApp via the
/// Messaging API). Treated as bytes — an oversized message is one Twilio
/// itself would reject or silently segment; we truncate rather than let
/// that happen unpredictably.
pub const TWILIO_TEXT_MAX_BYTES: usize = 1600;

const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded Twilio's 1600-byte limit]";

/// Rendered Twilio message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    pub text: String,
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// Zero renderable components after deferring everything that isn't
    /// Text or Narration.
    EmptyAfterRender,
}

/// Try to render `result` as a Twilio message. Returns `None` when the
/// result isn't an A2UI surface — caller falls back to the bare-text
/// path. Returns `Some(Err(...))` when the result IS a surface but
/// renders to nothing usable.
pub fn try_render_surface(
    result: &serde_json::Value,
) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a `RenderedMessage`. Public so in-crate unit
/// tests can exercise the mapper without spinning the whole binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;

    for c in &surface.components {
        match c {
            Component::Text { value } => chunks.push(value.clone()),
            Component::Narration { text } => chunks.push(format!("_{text}_")),
            Component::Sources { items } => {
                if !items.is_empty() {
                    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
                    chunks.push(format!("Sources: {}", labels.join(" \u{b7} ")));
                }
            }
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored here.
            Component::Report { .. } => {}
            Component::Button { .. } => deferred_buttons += 1,
            Component::Selection { .. } => deferred_selections += 1,
            Component::Form { .. } => deferred_forms += 1,
            Component::Dashboard { .. } => deferred_dashboards += 1,
        }
    }

    let mut text = chunks.join("\n\n");
    let mut truncated = false;
    if text.len() > TWILIO_TEXT_MAX_BYTES {
        let budget = TWILIO_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
        let mut cut = budget.min(text.len());
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str(TRUNCATION_SENTINEL);
        truncated = true;
    }

    if text.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }

    Ok(RenderedMessage {
        text,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        deferred_dashboards,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::SourceItem;

    #[test]
    fn text_and_narration_render_joined() {
        let surface = Surface {
            components: vec![
                Component::Text {
                    value: "hello".to_string(),
                },
                Component::Narration {
                    text: "aside".to_string(),
                },
            ],
        };
        let r = render(&surface).expect("renders");
        assert_eq!(r.text, "hello\n\n_aside_");
        assert_eq!(r.deferred_buttons, 0);
    }

    #[test]
    fn button_selection_form_dashboard_are_deferred_and_counted() {
        let surface = Surface {
            components: vec![
                Component::Text {
                    value: "hi".to_string(),
                },
                Component::Button {
                    label: "Go".to_string(),
                    tool: "t".to_string(),
                    args: serde_json::json!({}),
                    resource: None,
                },
                Component::Selection {
                    prompt: "pick".to_string(),
                    options: vec![],
                    tool: "t".to_string(),
                    args_key: "x".to_string(),
                },
                Component::Form {
                    title: "f".to_string(),
                    fields: vec![],
                    submit_label: "Submit".to_string(),
                    tool: "t".to_string(),
                },
                Component::Dashboard {
                    title: "d".to_string(),
                    tiles: vec![],
                },
            ],
        };
        let r = render(&surface).expect("renders");
        assert_eq!(r.text, "hi");
        assert_eq!(r.deferred_buttons, 1);
        assert_eq!(r.deferred_selections, 1);
        assert_eq!(r.deferred_forms, 1);
        assert_eq!(r.deferred_dashboards, 1);
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let surface = Surface { components: vec![] };
        assert_eq!(render(&surface), Err(RenderError::EmptyAfterRender));
    }

    #[test]
    fn oversized_text_is_truncated_with_sentinel() {
        let surface = Surface {
            components: vec![Component::Text {
                value: "x".repeat(TWILIO_TEXT_MAX_BYTES + 500),
            }],
        };
        let r = render(&surface).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= TWILIO_TEXT_MAX_BYTES);
        assert!(r.text.ends_with("1600-byte limit]"));
    }

    #[test]
    fn sources_render_as_a_label_list() {
        let surface = Surface {
            components: vec![Component::Sources {
                items: vec![SourceItem {
                    label: "Doc A".to_string(),
                    resource: "ui://x/a".to_string(),
                }],
            }],
        };
        let r = render(&surface).expect("renders");
        assert!(r.text.contains("Sources: Doc A"));
    }
}
