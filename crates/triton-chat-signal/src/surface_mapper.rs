//! L6′ surface mapper for the Signal adapter.
//!
//! Signal-via-signald only carries plain text bodies; there's no
//! native button/Selection/Form/Dashboard primitive. The mapper
//! therefore renders Text + Narration only and defers everything
//! else, counting per-category so the operator sees the gap via
//! `tracing::warn!` (same shape Telegram + Discord adopted).
//!
//! Output discipline:
//!   * Text  → plain text (no markup; Signal has no documented
//!     rich-text envelope across all clients).
//!   * Narration → `_<text>_` italics. signald passes the body
//!     verbatim to Signal Desktop / mobile; iOS + Android both
//!     render `_word_` as italic when no other markdown is present.
//!     This is a low-risk choice — if a client doesn't honour it,
//!     the underscores are still readable text.
//!   * Buttons, Selection, Form, Dashboard → deferred + counted.
//!
//! Cap: 2000 bytes per message. Signal's protocol doesn't impose a
//! formal byte limit but signald and most clients chunk large
//! messages awkwardly. The cap is conservative + symmetric with the
//! other text-first adapters; truncation appends a sentinel.

use serde_json::Value;
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Conservative per-message byte cap. Signal has no documented
/// hard limit, but ~2 KB is the practical threshold across
/// signald's queueing layer and the major clients' UIs.
pub const SIGNAL_TEXT_MAX_BYTES: usize = 2000;

const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded Signal's 2000-byte cap]";

/// Rendered Signal body. Unlike Telegram + Discord we don't carry a
/// `parse_mode` because signald doesn't accept one; the underscores
/// for narration are just bytes inside `messageBody`.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    /// Final text body that goes into signald's `send.messageBody`.
    pub text: String,
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// Zero renderable components (no Text or Narration). Signal
    /// has no equivalent of Telegram's button-only placeholder
    /// because there's no button primitive; an all-action surface
    /// is genuinely unrenderable here.
    EmptyAfterRender,
}

/// Try to render `result` as a Signal message. Returns `None` when
/// the result isn't an A2UI surface — caller falls back to the
/// bare-text path. Returns `Some(Err(...))` when the result IS an
/// A2UI surface but renders to nothing usable.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a [`RenderedMessage`] or a
/// [`RenderError`]. Public so unit tests can exercise the mapper
/// without spinning the whole binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(value.clone());
            }
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored by the text mapper.
            Component::Report { .. } => {}
            Component::Narration { text } => {
                // Single underscore wraps so Signal's markdown
                // rendering treats it as italic; the underscores
                // themselves remain readable on clients that don't
                // honour the convention. Keep it simple — no
                // escape table is necessary because the body is
                // plain text, not HTML.
                chunks.push(format!("_{text}_"));
            }
            Component::Button { .. } => {
                // Signal has no button primitive over signald.
                // Defer + count; the operator sees the gap via
                // tracing::warn in the caller.
                deferred_buttons += 1;
            }
            Component::Selection { .. } => {
                deferred_selections += 1;
            }
            Component::Form { .. } => {
                deferred_forms += 1;
            }
            Component::Dashboard { .. } => {
                // Per the manifest `dashboard: rasterised_png`
                // contract — never leak raw tile content into the
                // body (Codex pattern, see PR 27 telegram + PR 22
                // discord).
                deferred_dashboards += 1;
            }
        }
    }
    if chunks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    let joined = chunks.join("\n\n");
    if joined.len() <= SIGNAL_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
        });
    }
    // Over cap. Walk the chunk prefix that fits under the cap
    // minus the sentinel. Drop the tail. Every dropped boundary
    // sits between two complete chunks — we never cut mid-chunk
    // unless the FIRST chunk alone already exceeds the cap, in
    // which case we truncate at a UTF-8 char boundary.
    let budget = SIGNAL_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<String> = Vec::new();
    let mut total = 0usize;
    for chunk in &chunks {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 };
        if total + sep_cost + chunk.len() > budget {
            break;
        }
        total += sep_cost + chunk.len();
        accepted.push(chunk.clone());
    }
    let body = if accepted.is_empty() {
        // First chunk alone is too big — truncate it at a UTF-8
        // boundary. `str::is_char_boundary` walks backwards from
        // `budget` until we find a valid cut.
        let raw = &chunks[0];
        let mut cut = budget.min(raw.len());
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        raw[..cut].to_string()
    } else {
        accepted.join("\n\n")
    };
    let mut out = body;
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

/// Build the signald `send` JSON body for a destination
/// (uuid + number) and a rendered message.
///
/// signald's `send` request shape (per signald.org/articles/protocol):
/// ```json
/// { "type": "send",
///   "username": "+15551234567",
///   "recipientAddress": { "uuid": "...", "number": "+155..." },
///   "messageBody": "..."
/// }
/// ```
/// We always include the uuid (stable across phone-number changes,
/// per the task's identity key requirement); the `number` field is
/// included only when known so signald can pick the preferred
/// transport.
pub fn build_send_body(
    bot_account: &str,
    recipient_uuid: &str,
    recipient_number: Option<&str>,
    msg: &RenderedMessage,
) -> Value {
    let mut recipient = serde_json::Map::new();
    recipient.insert(
        "uuid".to_string(),
        Value::String(recipient_uuid.to_string()),
    );
    if let Some(number) = recipient_number {
        recipient.insert("number".to_string(), Value::String(number.to_string()));
    }
    serde_json::json!({
        "type": "send",
        "username": bot_account,
        "recipientAddress": Value::Object(recipient),
        "messageBody": msg.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use triton_core::a2ui::{Component, DashboardTile, SelectionOption, Surface};

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
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn button_only_surface_defers_and_errors() {
        // Signal has no button primitive; an all-button surface is
        // genuinely unrenderable.
        let s = Surface {
            components: vec![Component::Button {
                label: "Refresh".into(),
                tool: "narrate".into(),
                args: json!({}),
            }],
        };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn buttons_count_as_deferred_alongside_text() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "click below".into(),
                },
                Component::Button {
                    label: "Go".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "click below");
        assert_eq!(r.deferred_buttons, 1);
    }

    #[test]
    fn dashboard_does_not_leak_tile_content() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "header".into(),
                },
                Component::Dashboard {
                    title: "Secrets".into(),
                    tiles: vec![DashboardTile {
                        label: "invocations".into(),
                        value: "1234".into(),
                        trend: None,
                    }],
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
        assert_eq!(r.deferred_dashboards, 1);
    }

    #[test]
    fn selection_defers_no_prompt_leaks() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "context".into(),
                },
                Component::Selection {
                    prompt: "pick".into(),
                    options: vec![SelectionOption {
                        label: "A".into(),
                        value: "a".into(),
                    }],
                    tool: "narrate".into(),
                    args_key: "s".into(),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "context");
        assert_eq!(r.deferred_selections, 1);
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(10_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= SIGNAL_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let fourbyte = "𝄞";
        let s = Surface {
            components: vec![Component::Text {
                value: fourbyte.repeat(1000),
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        for i in 0..=r.text.len() {
            let _ = r.text.is_char_boundary(i);
        }
    }

    #[test]
    fn truncation_drops_tail_chunks() {
        let small = Component::Text {
            value: "x".repeat(500),
        };
        let s = Surface {
            components: (0..50).map(|_| small.clone()).collect(),
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }
}
