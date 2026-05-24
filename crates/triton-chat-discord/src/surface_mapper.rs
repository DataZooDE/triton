//! Discord L6' surface mapper. Same Surface input as the Telegram
//! mapper; output is a Discord interaction-response body:
//!
//! ```json
//! { "type": 4,
//!   "data": { "content": "<markdown>",
//!             "components": [ { "type": 1,
//!                                "components": [ { "type": 2, ... } ] } ] } }
//! ```
//!
//! Differences from Telegram:
//!
//! * Discord parses Discord-flavoured Markdown, not HTML. Narration
//!   renders as `*text*` (italic) and Form/Dashboard titles as
//!   `**text**` (bold). Markdown escaping is mandatory for any
//!   user-supplied text — backslash-escape every Markdown
//!   metacharacter.
//! * Buttons are `components v2`: an Action Row (`type: 1`)
//!   containing one or more Button (`type: 2`) entries with
//!   `style: 1` (primary), `label`, and `custom_id`. Discord's
//!   custom_id is up to 100 bytes; we keep the 64-byte
//!   correlation-token cap from PR 21 so the same token works on
//!   both adapters.
//! * Discord's per-message content cap is 2000 characters
//!   (UTF-16 code units; we use byte-count as a conservative
//!   proxy). The truncation strategy is the same as Telegram's
//!   PR 20: cut between chunks first, raw-text inside a single
//!   oversized component as a last resort.

use serde_json::{Value, json};
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Discord `data.content` hard limit. Documented as 2000 chars
/// (UTF-16 code units); we treat as bytes (conservative).
pub const DISCORD_CONTENT_MAX_BYTES: usize = 2000;

const TRUNCATION_SENTINEL: &str = "\n\n*(truncated — exceeded Discord's 2000-byte content cap)*";

const BUTTON_ONLY_PLACEHOLDER: &str = "Choose an option:";

#[derive(Debug, Clone)]
pub struct RenderedInteraction {
    pub content: String,
    pub components: Option<Value>,
    pub deferred_buttons: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    EmptyAfterRender,
}

pub fn try_render_surface(
    result: &Value,
    correlation_key: &[u8],
) -> Option<Result<RenderedInteraction, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface, correlation_key))
}

pub fn render(
    surface: &Surface,
    correlation_key: &[u8],
) -> Result<RenderedInteraction, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut button_row: Vec<Value> = Vec::new();
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(md_escape(value));
            }
            Component::Narration { text } => {
                chunks.push(format!("*{}*", md_escape(text)));
            }
            Component::Button { label, tool, args } => {
                match triton_correlation::encode(tool, args, correlation_key) {
                    Ok(token) => {
                        button_row.push(json!({
                            "type": 2,
                            "style": 1,
                            "label": label,
                            "custom_id": token,
                        }));
                    }
                    Err(_) => {
                        deferred_buttons += 1;
                    }
                }
            }
            // Selection / Form: render the prompt/title as a
            // chunk; full components-v2 select-menu / modal
            // mapping lands in a later PR (deferred action).
            Component::Selection {
                prompt, options, ..
            } => {
                let opts: Vec<String> = options.iter().map(|o| md_escape(&o.label)).collect();
                chunks.push(format!("{}\n{}", md_escape(prompt), opts.join(" | ")));
                deferred_buttons += 1;
            }
            Component::Form { title, fields, .. } => {
                let names: Vec<String> = fields.iter().map(|f| md_escape(&f.label)).collect();
                chunks.push(format!("**{}**\n{}", md_escape(title), names.join(", ")));
                deferred_buttons += 1;
            }
            Component::Dashboard { title, tiles } => {
                let mut lines = vec![format!("**{}**", md_escape(title))];
                for t in tiles {
                    let trend = t
                        .trend
                        .as_deref()
                        .map(|x| format!(" ({})", md_escape(x)))
                        .unwrap_or_default();
                    lines.push(format!(
                        "• {}: {}{}",
                        md_escape(&t.label),
                        md_escape(&t.value),
                        trend,
                    ));
                }
                chunks.push(lines.join("\n"));
            }
        }
    }

    if chunks.is_empty() && button_row.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    if chunks.is_empty() {
        chunks.push(BUTTON_ONLY_PLACEHOLDER.into());
    }

    let joined = chunks.join("\n\n");
    let (content, truncated) = if joined.len() <= DISCORD_CONTENT_MAX_BYTES {
        (joined, false)
    } else {
        truncate_chunks(&chunks)
    };

    let components = if button_row.is_empty() {
        None
    } else {
        Some(json!([{ "type": 1, "components": button_row }]))
    };

    Ok(RenderedInteraction {
        content,
        components,
        deferred_buttons,
        truncated,
    })
}

fn truncate_chunks(chunks: &[String]) -> (String, bool) {
    let budget = DISCORD_CONTENT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in chunks {
        let sep = if accepted.is_empty() { 0 } else { 2 };
        if total + sep + chunk.len() > budget {
            break;
        }
        total += sep + chunk.len();
        accepted.push(chunk.as_str());
    }
    let body = if !accepted.is_empty() {
        accepted.join("\n\n")
    } else {
        // First chunk alone too big — UTF-8-safe truncate.
        let raw = &chunks[0];
        let mut end = budget.min(raw.len());
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        raw[..end].to_string()
    };
    let mut out = body;
    out.push_str(TRUNCATION_SENTINEL);
    (out, true)
}

/// Backslash-escape every Discord Markdown metacharacter. Discord's
/// Markdown is simpler than CommonMark — the active characters are
/// `* _ ~ ` > | \`. Escaping with `\` is the only safe pattern; any
/// HTML-style escape attempt is interpreted literally.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' | '*' | '_' | '~' | '`' | '>' | '|' | '#' | '-' | '+' | '.' | '!' | '[' | ']'
            | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

pub fn build_interaction_response(rendered: &RenderedInteraction) -> Value {
    let mut data = json!({ "content": rendered.content });
    if let Some(components) = &rendered.components {
        data.as_object_mut()
            .unwrap()
            .insert("components".into(), components.clone());
    }
    json!({
        "type": 4,
        "data": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use triton_core::a2ui::{Component, Surface};

    const TEST_KEY: &[u8] = b"test-correlation-key-32-bytes!!!";

    #[test]
    fn narration_renders_as_italic_markdown() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "hello".into(),
                },
                Component::Narration {
                    text: "footnote".into(),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.content, "hello\n\n*footnote*");
        assert!(r.components.is_none());
    }

    #[test]
    fn markdown_special_chars_are_escaped() {
        let s = Surface {
            components: vec![Component::Text {
                value: "a*b_c~d".into(),
            }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        // Each metacharacter gets a backslash prefix.
        assert!(r.content.contains(r"a\*b\_c\~d"));
    }

    #[test]
    fn button_renders_as_action_row() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "label".into(),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        let components = r.components.expect("components present");
        let rows = components.as_array().expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["type"], 1); // ACTION_ROW
        let buttons = rows[0]["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0]["type"], 2); // BUTTON
        assert_eq!(buttons[0]["label"], "Refresh");
        let token = buttons[0]["custom_id"]
            .as_str()
            .expect("custom_id is a string");
        let (tool, _) = triton_correlation::decode(token, TEST_KEY).expect("token verifies");
        assert_eq!(tool, "narrate");
    }

    #[test]
    fn button_only_surface_synthesises_placeholder() {
        let s = Surface {
            components: vec![Component::Button {
                label: "Refresh".into(),
                tool: "narrate".into(),
                args: json!({}),
            }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.content, BUTTON_ONLY_PLACEHOLDER);
        assert!(r.components.is_some());
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(
            render(&s, TEST_KEY),
            Err(RenderError::EmptyAfterRender)
        ));
    }

    #[test]
    fn oversized_content_truncates_with_sentinel() {
        let big = "x".repeat(5_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        assert!(r.content.len() <= DISCORD_CONTENT_MAX_BYTES);
        assert!(r.content.ends_with(TRUNCATION_SENTINEL));
    }
}
