//! v0.2 PR 33 — L6' surface mapper for Google Chat.
//!
//! Scope: text + narration only. Buttons / Selection / Form /
//! Dashboard defer with counters (architecture.md §8.7 — interactive
//! Cards are a follow-up PR).
//!
//! Output discipline:
//!   * Text → plain text, no escaping (Google Chat's default
//!     `text` field is plain — Markdown-style escaping would
//!     just show backslashes to the user).
//!   * Narration → wrapped in single-asterisk italic per Google
//!     Chat's text formatting rules (`*italic*`), the closest
//!     analogue to Telegram's `<i>` and Discord's `*`. We escape
//!     any bare `*` in the user text so the italic markers stay
//!     balanced.
//!   * Button / Selection / Form / Dashboard → deferred, counted.
//!
//! Truncation strategy mirrors Telegram's PR 20: chunks are
//! pre-rendered, joined with `\n\n`, and if the total exceeds
//! [`GOOGLE_CHAT_TEXT_MAX_BYTES`] we walk the head-prefix that fits
//! and append a sentinel. Cuts always land between chunks, never
//! mid-component, so italic markers stay balanced.

use serde_json::Value;
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Google Chat caps message text at ~32,000 chars; we use byte
/// count as a conservative proxy.
pub const GOOGLE_CHAT_TEXT_MAX_BYTES: usize = 32_000;

const TRUNCATION_SENTINEL: &str =
    "\n\n[truncated — content exceeded Google Chat's 32,000-byte limit]";

#[derive(Debug, Clone)]
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
    /// The Surface had zero renderable components (no Text or
    /// Narration). Action-only Surfaces (Button/Form/Selection) land
    /// here for PR 33 because Cards aren't wired yet.
    EmptyAfterRender,
}

/// Try to render `result` as a Google Chat message. Returns `None`
/// when the result isn't an A2UI surface (caller falls back to
/// bare text). Returns `Some(Err(...))` when the result IS a
/// Surface but has no renderable content.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

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
            Component::Narration { text } => {
                chunks.push(format!("*{}*", escape_italic(text)));
            }
            Component::Button { .. } => {
                deferred_buttons += 1;
            }
            Component::Selection { .. } => {
                deferred_selections += 1;
            }
            Component::Form { .. } => {
                deferred_forms += 1;
            }
            Component::Dashboard { .. } => {
                deferred_dashboards += 1;
            }
        }
    }
    if chunks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    let joined = chunks.join("\n\n");
    if joined.len() <= GOOGLE_CHAT_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
        });
    }
    // Over cap. Walk the head-prefix that fits under
    // `cap - sentinel`. Stop at the largest chunk boundary that
    // fits so the italic markers in a Narration chunk stay
    // balanced (we never cut inside a chunk).
    let budget = GOOGLE_CHAT_TEXT_MAX_BYTES.saturating_sub(TRUNCATION_SENTINEL.len());
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in &chunks {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 }; // "\n\n"
        if total + sep_cost + chunk.len() > budget {
            break;
        }
        total += sep_cost + chunk.len();
        accepted.push(chunk.as_str());
    }
    let body = if accepted.is_empty() {
        // Even the first chunk is too large. Cut at the largest
        // char boundary that fits under the budget so we never
        // emit malformed UTF-8 (and never split a 4-byte
        // codepoint). The first chunk is either raw Text or
        // `*Narration*`; in either case a clean prefix is
        // safe — an italic-marker imbalance is tolerable here
        // since the sentinel is the visible signal that this is
        // a truncation artefact.
        truncate_to_char_boundary(&chunks[0], budget).to_string()
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

/// Escape `*` in narration text so the wrapping `*...*` italic
/// markers don't unbalance. Google Chat's text formatting treats
/// `*` as the italic delimiter; the rest of the syntax
/// (`_underline_`, `~strikethrough~`, ``` `code` ```) we leave
/// alone because the mapper only emits italics.
fn escape_italic(s: &str) -> String {
    s.replace('*', "\\*")
}

/// UTF-8-safe truncation at the largest char boundary `<= max`.
fn truncate_to_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Wrap a plain-text reply in the JSON envelope Google Chat expects on
/// the synchronous-response path. The shape depends on how the Chat app
/// is deployed:
///
///   * **classic / dedicated Chat app** (`workspace_addon = false`) — a
///     bare Chat `Message`: `{ "text": … }`.
///   * **Workspace Add-on** (`workspace_addon = true`) — the message
///     nested in a host-app data action:
///     `{ "hostAppDataAction": { "chatDataAction": { "createMessageAction":
///        { "message": { "text": … } } } } }`.
///
/// The add-on runtime parses the response as `RenderActions` /
/// `DataActions` / `Card` and rejects a bare `{text}` ("Cannot find
/// field: text"), so the two cannot share one shape. The caller picks
/// the flavor from the verified inbound token
/// ([`crate::jwt_verifier::GoogleChatClaims::is_workspace_addon`]) — an
/// add-on always signs with the Workspace Add-ons service agent, so the
/// token is the authoritative signal and no operator config is needed.
pub fn text_reply_body(text: &str, workspace_addon: bool) -> Value {
    if workspace_addon {
        serde_json::json!({
            "hostAppDataAction": {
                "chatDataAction": {
                    "createMessageAction": { "message": { "text": text } }
                }
            }
        })
    } else {
        serde_json::json!({ "text": text })
    }
}

/// Build the inline-response JSON body Google Chat expects. Google Chat
/// reads the webhook's HTTP 200 response body and delivers the rendered
/// text to the space. `workspace_addon` selects the reply envelope (see
/// [`text_reply_body`]).
pub fn build_inline_response(msg: &RenderedMessage, workspace_addon: bool) -> Value {
    text_reply_body(&msg.text, workspace_addon)
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
        assert_eq!(r.text, "hello\n\n*a footnote*");
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn text_only_renders_plain() {
        let s = Surface {
            components: vec![Component::Text {
                value: "plain".into(),
            }],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "plain");
    }

    #[test]
    fn buttons_selections_forms_dashboards_defer() {
        use triton_core::a2ui::{DashboardTile, FormField, FormFieldKind, SelectionOption};
        let s = Surface {
            components: vec![
                Component::Text { value: "x".into() },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: serde_json::json!({}),
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
                    title: "Title".into(),
                    fields: vec![FormField {
                        name: "n".into(),
                        label: "L".into(),
                        kind: FormFieldKind::String,
                        required: true,
                    }],
                    submit_label: "Go".into(),
                    tool: "echo".into(),
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
        assert_eq!(r.text, "x");
        assert_eq!(r.deferred_buttons, 1);
        assert_eq!(r.deferred_selections, 1);
        assert_eq!(r.deferred_forms, 1);
        assert_eq!(r.deferred_dashboards, 1);
        // Tile content must NEVER leak (architecture.md §8.7).
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
    }

    #[test]
    fn empty_surface_is_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn button_only_surface_defers_with_error() {
        // PR 33: with no Cards yet, a button-only surface has
        // nothing to render and the courier must NOT post.
        let s = Surface {
            components: vec![Component::Button {
                label: "Click".into(),
                tool: "narrate".into(),
                args: serde_json::json!({}),
            }],
        };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn narration_asterisks_are_escaped() {
        // Bare `*` in narration text would unbalance the italic
        // wrapping. Escape so the user-visible string is what the
        // tool actually emitted (with literal asterisks rendered
        // as backslash-asterisk per Google Chat's escape syntax).
        let s = Surface {
            components: vec![Component::Narration {
                text: "danger * zone".into(),
            }],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "*danger \\* zone*");
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(40_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= GOOGLE_CHAT_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn truncation_drops_tail_chunks_when_head_fits() {
        let small = Component::Text {
            value: "y".repeat(2_000),
        };
        let s = Surface {
            components: (0..50).map(|_| small.clone()).collect(),
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
        let body = r.text.trim_end_matches(TRUNCATION_SENTINEL);
        for chunk in body.split("\n\n") {
            assert!(
                chunk.is_empty() || chunk.chars().all(|c| c == 'y'),
                "expected only complete chunks; found: {chunk:?}"
            );
        }
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        // 4-byte UTF-8 codepoint so a naive byte-slice would land
        // mid-sequence; verify the cut lands on a char boundary.
        let fourbyte = "\u{1D11E}"; // U+1D11E (𝄞), 4 bytes
        let s = Surface {
            components: vec![Component::Text {
                value: fourbyte.repeat(10_000),
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        for i in 0..=r.text.len() {
            // is_char_boundary returns true at any valid index in a
            // valid Rust String; this just confirms the cut didn't
            // produce invalid UTF-8 internally.
            let _ = r.text.is_char_boundary(i);
        }
    }

    #[test]
    fn multi_component_ordering_is_preserved() {
        let s = Surface {
            components: vec![
                Component::Text { value: "1".into() },
                Component::Narration { text: "2".into() },
                Component::Text { value: "3".into() },
                Component::Narration { text: "4".into() },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "1\n\n*2*\n\n3\n\n*4*");
    }

    #[test]
    fn classic_reply_is_a_bare_text_message() {
        let body = text_reply_body("hello", false);
        assert_eq!(body, serde_json::json!({ "text": "hello" }));
    }

    #[test]
    fn workspace_addon_reply_is_a_host_app_data_action() {
        // A Workspace Add-on rejects a bare {text}; the message must be
        // nested in hostAppDataAction → chatDataAction → createMessageAction.
        let body = text_reply_body("hello", true);
        assert_eq!(
            body,
            serde_json::json!({
                "hostAppDataAction": {
                    "chatDataAction": {
                        "createMessageAction": { "message": { "text": "hello" } }
                    }
                }
            })
        );
        // The add-on envelope must NOT carry a top-level `text` (that's
        // exactly what the add-on runtime rejects).
        assert!(body.get("text").is_none());
    }

    #[test]
    fn build_inline_response_selects_envelope_by_flavor() {
        let msg = RenderedMessage {
            text: "answer".into(),
            deferred_buttons: 0,
            deferred_selections: 0,
            deferred_forms: 0,
            deferred_dashboards: 0,
            truncated: false,
        };
        assert_eq!(
            build_inline_response(&msg, false),
            serde_json::json!({ "text": "answer" })
        );
        assert_eq!(
            build_inline_response(&msg, true)["hostAppDataAction"]["chatDataAction"]["createMessageAction"]
                ["message"]["text"],
            serde_json::json!("answer")
        );
    }
}
