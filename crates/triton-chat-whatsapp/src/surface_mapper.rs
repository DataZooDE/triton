//! v0.2 PR 31 — L6' surface mapper for WhatsApp Cloud API.
//!
//! Takes the canonical `triton_core::a2ui::Surface` and renders it
//! into a WhatsApp `messages` body. PR 31's scope is intentionally
//! narrow: text + narration only. Buttons, Selection, Form, and
//! Dashboard components are counted into per-category `deferred_*`
//! fields so the operator can see the gap via `tracing::warn`, but
//! they are NOT rendered through interactive primitives — that
//! lands in PR 32.
//!
//! Output discipline:
//!   * text → plain text (WhatsApp doesn't speak HTML/Markdown in
//!     the same parse-mode way Telegram does; we keep it simple
//!     until PR 32 introduces native primitives).
//!   * narration → `_<text>_` (WhatsApp's documented italic markup;
//!     applied client-side only, no risk of breaking parsing if it
//!     fails to render).
//!   * buttons/selection/form/dashboard → deferred.
//!
//! Truncation strategy mirrors Telegram's: cut between components
//! first, raw-text-budget fallback for a single oversized chunk.
//! The cap is WhatsApp's documented 4096 chars; we treat it as
//! bytes (any UTF-8 string ≤ 4096 bytes is ≤ 4096 UTF-16 code
//! units, so a byte cap only ever rejects messages WhatsApp itself
//! would also reject).

use serde_json::{Value, json};
use triton_core::a2ui::{Component, Surface, extract_surface};

/// WhatsApp `text.body` hard limit. Same byte-conservative reading
/// as Telegram's TELEGRAM_TEXT_MAX_BYTES (UTF-8 length ≤ UTF-16
/// length).
pub const WHATSAPP_TEXT_MAX_BYTES: usize = 4096;

/// Sentinel appended when we truncate to fit
/// [`WHATSAPP_TEXT_MAX_BYTES`]. Bracketed so it's visibly an
/// adapter artefact, not the tool's output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded WhatsApp's 4096-byte limit]";

/// Rendered WhatsApp message body.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    /// Buttons in PR 31 cannot ship as interactive primitives; we
    /// count them so the operator-visible tracing::warn line names
    /// the category. PR 32 lifts this restriction.
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// Zero renderable components after deferring everything that
    /// isn't Text or Narration.
    EmptyAfterRender,
}

/// Try to render `result` as a WhatsApp message. Returns `None`
/// when the result isn't an A2UI surface — caller falls back to
/// the bare-text path. Returns `Some(Err(...))` when the result IS
/// an A2UI surface but renders to nothing usable.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a `RenderedMessage` or a
/// `RenderError`. Public so the in-crate unit tests can exercise
/// the mapper without spinning the whole binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    // Track which chunks came from raw text so the single-chunk
    // fallback path can re-render at a smaller budget without
    // corrupting markup. Narration carries `_..._` wrappers we
    // mustn't split mid-token.
    let mut raw_sources: Vec<Option<RawChunk>> = Vec::new();

    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(value.clone());
                raw_sources.push(Some(RawChunk {
                    kind: RawKind::Text,
                    raw: value.clone(),
                }));
            }
            Component::Narration { text } => {
                chunks.push(format!("_{text}_"));
                raw_sources.push(Some(RawChunk {
                    kind: RawKind::Narration,
                    raw: text.clone(),
                }));
            }
            Component::Button { .. } => {
                // PR 31 scope-limit: interactive primitives defer
                // until PR 32. The Button's tool/args MUST NOT leak
                // into the text body — that would let the user
                // re-execute by typing.
                deferred_buttons += 1;
            }
            Component::Selection { .. } => {
                deferred_selections += 1;
            }
            Component::Form { .. } => {
                deferred_forms += 1;
            }
            Component::Dashboard { .. } => {
                // Same rule as Telegram PR 27 and Discord PR 22:
                // the manifest declares `dashboard: rasterised_png`,
                // so we never leak raw tile content.
                deferred_dashboards += 1;
            }
        }
    }

    if chunks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }

    let joined = chunks.join("\n\n");
    if joined.len() <= WHATSAPP_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
        });
    }

    // Over cap. Walk the chunk prefix that fits under
    // `cap - sentinel`, leaving every cut on a between-component
    // boundary so narration `_..._` markup stays balanced.
    let budget = WHATSAPP_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
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

    // Even the first chunk is too large. We can budget-truncate
    // the raw text and re-wrap. WhatsApp's `_..._` markup spans the
    // full narration body so we re-emit the wrapper around the
    // truncated raw.
    let first_raw = raw_sources[0].clone();
    let out = match first_raw {
        Some(RawChunk {
            kind: RawKind::Text,
            raw,
        }) => {
            let trimmed = budget_raw(&raw, budget);
            let mut s = trimmed.to_string();
            s.push_str(TRUNCATION_SENTINEL);
            s
        }
        Some(RawChunk {
            kind: RawKind::Narration,
            raw,
        }) => {
            // Narration adds `_` + `_` = 2 bytes of wrapping.
            let inner_budget = budget.saturating_sub(2);
            let trimmed = budget_raw(&raw, inner_budget);
            let mut s = format!("_{trimmed}_");
            s.push_str(TRUNCATION_SENTINEL);
            s
        }
        None => TRUNCATION_SENTINEL.trim_start().to_string(),
    };
    Ok(RenderedMessage {
        text: out,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        deferred_dashboards,
        truncated: true,
    })
}

#[derive(Clone)]
struct RawChunk {
    kind: RawKind,
    raw: String,
}

#[derive(Clone, Copy)]
enum RawKind {
    Text,
    Narration,
}

/// Walk `raw` char-by-char so a cut never lands mid-codepoint.
fn budget_raw(raw: &str, max_bytes: usize) -> &str {
    let mut total = 0usize;
    let mut end = 0usize;
    for (i, c) in raw.char_indices() {
        let n = c.len_utf8();
        if total + n > max_bytes {
            break;
        }
        total += n;
        end = i + n;
    }
    &raw[..end]
}

/// Build the WhatsApp `messages` request body for `to` + the
/// rendered text. Lives next to the renderer so the rendering +
/// serialisation stay paired.
pub fn build_messages_body(to: &str, msg: &RenderedMessage) -> Value {
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "text",
        "text": {
            "body": msg.text,
            "preview_url": false,
        }
    })
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
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn buttons_selection_form_dashboard_are_deferred_not_rendered() {
        use serde_json::json;
        use triton_core::a2ui::{DashboardTile, FormField, FormFieldKind, SelectionOption};
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "preamble".into(),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: json!({}),
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
                Component::Form {
                    title: "form".into(),
                    fields: vec![FormField {
                        name: "name".into(),
                        label: "Name".into(),
                        kind: FormFieldKind::String,
                        required: false,
                    }],
                    submit_label: "Send".into(),
                    tool: "narrate".into(),
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
        // None of the interactive components contribute to text.
        assert_eq!(r.text, "preamble");
        assert_eq!(r.deferred_buttons, 1);
        assert_eq!(r.deferred_selections, 1);
        assert_eq!(r.deferred_forms, 1);
        assert_eq!(r.deferred_dashboards, 1);
        // Dashboard tile content MUST NEVER leak into the body.
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn button_only_surface_defers_to_empty_after_render() {
        // No text-bearing components → nothing to ship via WhatsApp's
        // text channel. PR 32 will rewire this through interactive
        // primitives; for now it's an explicit drop.
        use serde_json::json;
        let s = Surface {
            components: vec![Component::Button {
                label: "Click".into(),
                tool: "narrate".into(),
                args: json!({}),
            }],
        };
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
        assert!(r.text.len() <= WHATSAPP_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let fourbyte = "𝄞"; // U+1D11E, 4 bytes
        let s = Surface {
            components: vec![Component::Text {
                value: fourbyte.repeat(2000),
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        // The String type guarantees valid UTF-8; spot-check char
        // boundaries don't panic when iterated.
        for i in 0..=r.text.len() {
            let _ = r.text.is_char_boundary(i);
        }
    }

    #[test]
    fn narration_truncation_keeps_underscores_balanced() {
        let big = "n".repeat(10_000);
        let s = Surface {
            components: vec![Component::Narration { text: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        // The italic wrapper must remain matched: one opener + one
        // closer in the body. We tolerate raw text that itself
        // happens to contain `_` (it doesn't for an `n`-only fill).
        let opens = r.text.chars().filter(|c| *c == '_').count();
        assert_eq!(opens, 2, "expected balanced _..._ wrapper; got: {}", r.text);
    }

    #[test]
    fn build_messages_body_shape_matches_cloud_api() {
        let msg = RenderedMessage {
            text: "hello".into(),
            deferred_buttons: 0,
            deferred_selections: 0,
            deferred_forms: 0,
            deferred_dashboards: 0,
            truncated: false,
        };
        let body = build_messages_body("491234567", &msg);
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["recipient_type"], "individual");
        assert_eq!(body["to"], "491234567");
        assert_eq!(body["type"], "text");
        assert_eq!(body["text"]["body"], "hello");
        assert_eq!(body["text"]["preview_url"], false);
    }
}
