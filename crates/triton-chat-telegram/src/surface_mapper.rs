//! v0.2 PR 19 — L6' surface mapper for Telegram.
//!
//! Takes the canonical `triton_core::a2ui::Surface` a tool returns
//! and renders it into a Telegram `sendMessage` body. The mapper
//! honours the manifest's per-component `degrade` table (see
//! `doc/architecture.md` §L6′) — but PR 19 ships only the
//! passthrough cases (`text`, `narration`). Buttons defer until
//! the HMAC correlation-token PR lands (we can't ship arbitrary
//! `(tool, args)` through Telegram's 64-byte `callback_data`
//! without a signed correlation token).
//!
//! Output discipline:
//!   * narration → `<i>...</i>` with `parse_mode: "HTML"` set on
//!     the sendMessage body.
//!   * text → plain text, HTML-escaped (because we're already in
//!     HTML mode for the narration).
//!   * buttons → audited as deferred (tracing::warn) but the
//!     surrounding text + narration still ship.
//!
//! HTML escaping is mandatory: a tool that emits `<script>` would
//! otherwise inject HTML into the post-back, and Telegram's
//! `parse_mode: "HTML"` parser would 400 the whole request on any
//! stray `<` or `&`.

use serde_json::{Value, json};
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Rendered Telegram body. `parse_mode` is set when the rendering
/// uses HTML markers (narration as italics); plain-text-only
/// renders leave it `None` so we don't gratuitously parse.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    pub parse_mode: Option<&'static str>,
    /// Number of `Component::Button` entries we encountered but
    /// did not render. The caller logs this so deferred buttons
    /// don't silently vanish; the count is also useful for
    /// metrics when the next PR ships correlation tokens.
    pub deferred_buttons: usize,
}

/// Try to render `result` as a Telegram message. Returns `None`
/// when the result isn't an A2UI surface — caller falls back to
/// the bare-text path from PR 18.
pub fn try_render_surface(result: &Value) -> Option<RenderedMessage> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a `RenderedMessage`. Public so the
/// unit tests in this crate can exercise the mapper without
/// spinning the whole binary.
pub fn render(surface: &Surface) -> RenderedMessage {
    let mut parts: Vec<String> = Vec::new();
    let mut has_html_markers = false;
    let mut deferred_buttons = 0usize;
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                parts.push(html_escape(value));
            }
            Component::Narration { text } => {
                parts.push(format!("<i>{}</i>", html_escape(text)));
                has_html_markers = true;
            }
            Component::Button { .. } => {
                deferred_buttons += 1;
            }
        }
    }
    RenderedMessage {
        text: parts.join("\n\n"),
        parse_mode: if has_html_markers { Some("HTML") } else { None },
        deferred_buttons,
    }
}

/// HTML-escape per Telegram's parse_mode HTML rules — only `<`,
/// `>`, `&` need replacing (quotes don't, because we're not in an
/// HTML attribute context). Keep the order: `&` first so we don't
/// double-escape entities we just produced.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the `sendMessage` JSON body from a `RenderedMessage`.
/// Lives next to the renderer so the rendering + serialisation
/// stay paired (no caller has to remember to set parse_mode).
pub fn build_send_message_body(chat_id: i64, msg: &RenderedMessage) -> Value {
    let mut body = json!({
        "chat_id": chat_id,
        "text": msg.text,
    });
    if let Some(pm) = msg.parse_mode {
        body.as_object_mut()
            .unwrap()
            .insert("parse_mode".into(), Value::String(pm.to_string()));
    }
    body
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
        let r = render(&s);
        assert_eq!(r.text, "hello\n\n<i>a footnote</i>");
        assert_eq!(r.parse_mode, Some("HTML"));
        assert_eq!(r.deferred_buttons, 0);
    }

    #[test]
    fn text_only_omits_parse_mode() {
        let s = Surface {
            components: vec![Component::Text {
                value: "plain".into(),
            }],
        };
        let r = render(&s);
        assert_eq!(r.text, "plain");
        assert_eq!(r.parse_mode, None);
    }

    #[test]
    fn buttons_are_counted_not_rendered() {
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
        let r = render(&s);
        assert_eq!(r.deferred_buttons, 1);
        assert!(r.text.contains("label"));
        assert!(!r.text.contains("Refresh"));
    }

    #[test]
    fn html_special_chars_are_escaped() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "a < b & c > d".into(),
                },
                Component::Narration {
                    text: "x<i>y</i>z".into(),
                },
            ],
        };
        let r = render(&s);
        assert!(r.text.contains("a &lt; b &amp; c &gt; d"));
        assert!(r.text.contains("<i>x&lt;i&gt;y&lt;/i&gt;z</i>"));
    }
}
