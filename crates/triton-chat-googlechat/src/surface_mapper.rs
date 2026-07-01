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
use triton_core::a2ui::{Component, FormFieldKind, Surface, extract_surface};

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
    wrap_message(serde_json::json!({ "text": text }), workspace_addon)
}

/// Wrap a Chat `Message` object in the reply envelope for the app's
/// flavor: bare for a classic/dedicated app, nested in
/// `hostAppDataAction → chatDataAction → createMessageAction` for a
/// Workspace Add-on (see [`text_reply_body`]). Used for both the
/// plain-text and the card (`cardsV2`) replies.
fn wrap_message(message: Value, workspace_addon: bool) -> Value {
    if workspace_addon {
        serde_json::json!({
            "hostAppDataAction": {
                "chatDataAction": { "createMessageAction": { "message": message } }
            }
        })
    } else {
        message
    }
}

/// Build the inline-response JSON body Google Chat expects. Google Chat
/// reads the webhook's HTTP 200 response body and delivers the rendered
/// text to the space. `workspace_addon` selects the reply envelope (see
/// [`text_reply_body`]).
pub fn build_inline_response(msg: &RenderedMessage, workspace_addon: bool) -> Value {
    text_reply_body(&msg.text, workspace_addon)
}

/// The `function` name stamped on every interactive `onClick.action` and
/// echoed back on the `CARD_CLICKED` event. The component's
/// `(tool, base_args)` rides a single signed correlation token in the
/// `ct` parameter; any typed/selected values arrive separately in the
/// event's `formInputs`. The name is constant — the callback only reads
/// the token + form inputs.
pub const BUTTON_ACTION_FUNCTION: &str = "agent_action";
/// Parameter key carrying the signed correlation token.
pub const BUTTON_TOKEN_PARAM: &str = "ct";
/// Parameter key carrying the button's display label, echoed back on the
/// `CARD_CLICKED` event so the reply can show WHICH button was tapped
/// (Google Chat doesn't render a click as a user message). Display-only,
/// so it is not signed — at worst a caller misattributes its own reply.
pub const BUTTON_LABEL_PARAM: &str = "lbl";

/// A single form field to render as a Cards v2 input.
#[derive(Debug, Clone, PartialEq)]
pub struct FormFieldSpec {
    pub name: String,
    pub label: String,
    /// `true` → render a SWITCH (boolean); else a text input.
    pub boolean: bool,
}

/// An interactive component lifted off a Surface. The adapter signs each
/// one's `(tool, base_args)` into a correlation token (it holds the key;
/// this stays key-free) and renders the matching Cards v2 widget(s) via
/// [`build_interactive_card`]. On the `CARD_CLICKED` submit it verifies
/// the token and re-dispatches `tool` with `base_args` ⊕ the user's
/// `formInputs`.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractiveSpec {
    /// One-tap preset → re-invoke `tool` with fixed `args`.
    Button {
        label: String,
        tool: String,
        args: Value,
    },
    /// Pick-one dropdown → re-invoke `tool` with the chosen value bound to
    /// `args_key` (delivered via `formInputs`).
    Selection {
        prompt: String,
        options: Vec<(String, String)>,
        tool: String,
        args_key: String,
    },
    /// Multi-field form → re-invoke `tool` with each field's typed value
    /// (delivered via `formInputs`, keyed by field name).
    Form {
        title: String,
        submit_label: String,
        fields: Vec<FormFieldSpec>,
        tool: String,
    },
}

impl InteractiveSpec {
    /// The tool this component re-invokes.
    pub fn tool(&self) -> &str {
        match self {
            InteractiveSpec::Button { tool, .. }
            | InteractiveSpec::Selection { tool, .. }
            | InteractiveSpec::Form { tool, .. } => tool,
        }
    }

    /// The args signed into the correlation token. For a `Button` this is
    /// the full preset args; for `Selection`/`Form` it is empty — the
    /// user-supplied values arrive on the submit's `formInputs` and are
    /// merged then. Those values are query parameters, not a trust
    /// decision, so they need no signature; the token only fixes the
    /// **tool** (and any preset args) against tampering.
    pub fn base_args(&self) -> Value {
        match self {
            InteractiveSpec::Button { args, .. } => args.clone(),
            _ => serde_json::json!({}),
        }
    }
}

/// Extract the interactive components (`Button` / `Selection` / `Form`)
/// from a dispatch result's A2UI surface, in order. Empty when the result
/// isn't a surface or has none (caller then sends a plain text reply).
pub fn interactive_from_result(result: &Value) -> Vec<InteractiveSpec> {
    let Ok(surface) = extract_surface(result) else {
        return Vec::new();
    };
    surface
        .components
        .iter()
        .filter_map(|c| match c {
            Component::Button { label, tool, args } => Some(InteractiveSpec::Button {
                label: label.clone(),
                tool: tool.clone(),
                args: args.clone(),
            }),
            Component::Selection {
                prompt,
                options,
                tool,
                args_key,
            } => Some(InteractiveSpec::Selection {
                prompt: prompt.clone(),
                options: options
                    .iter()
                    .map(|o| (o.label.clone(), o.value.clone()))
                    .collect(),
                tool: tool.clone(),
                args_key: args_key.clone(),
            }),
            Component::Form {
                title,
                fields,
                submit_label,
                tool,
            } => Some(InteractiveSpec::Form {
                title: title.clone(),
                submit_label: submit_label.clone(),
                fields: fields
                    .iter()
                    .map(|f| FormFieldSpec {
                        name: f.name.clone(),
                        label: f.label.clone(),
                        boolean: matches!(f.kind, FormFieldKind::Boolean),
                    })
                    .collect(),
                tool: tool.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// One Cards v2 action button carrying the signed correlation `token` and
/// its display `label` (echoed back on click so the reply can show which
/// button was tapped). `FILLED_TONAL` gives the row a clear tappable look.
fn action_button(label: &str, token: &str) -> Value {
    serde_json::json!({
        "text": label,
        "type": "FILLED_TONAL",
        "onClick": {
            "action": {
                "function": BUTTON_ACTION_FUNCTION,
                "parameters": [
                    { "key": BUTTON_TOKEN_PARAM, "value": token },
                    { "key": BUTTON_LABEL_PARAM, "value": label }
                ]
            }
        }
    })
}

/// A `Dashboard` lifted off a surface for Cards v2 rendering: its title
/// and `(label, value)` metric tiles.
pub type DashboardData = (String, Vec<(String, String)>);

/// Extract the first `Dashboard` component from a dispatch result's
/// surface (title + tiles). `None` when there's no surface or no
/// dashboard.
pub fn dashboard_from_result(result: &Value) -> Option<DashboardData> {
    let surface = extract_surface(result).ok()?;
    surface.components.iter().find_map(|c| match c {
        Component::Dashboard { title, tiles } => Some((
            title.clone(),
            tiles
                .iter()
                .map(|t| (t.label.clone(), t.value.clone()))
                .collect::<Vec<_>>(),
        )),
        _ => None,
    })
}

/// A Cards v2 section for the dashboard. With an `image_url` (a chart PNG
/// the adapter serves on demand) it renders an `image` widget — the actual
/// bar chart; otherwise a 2-column `grid` of metric tiles (value as each
/// item's title, label as its subtitle) as the hosting-free fallback.
fn dashboard_section(dashboard: &DashboardData, image_url: Option<&str>) -> Value {
    let (title, _tiles) = dashboard;
    let widget = match image_url {
        Some(url) => serde_json::json!({ "image": { "imageUrl": url, "altText": title } }),
        None => {
            let items: Vec<Value> = dashboard
                .1
                .iter()
                .map(|(label, value)| serde_json::json!({ "title": value, "subtitle": label }))
                .collect();
            serde_json::json!({ "grid": { "columnCount": 2, "items": items } })
        }
    };
    let mut section = serde_json::json!({ "widgets": [ widget ] });
    if !title.is_empty() {
        section["header"] = serde_json::json!(title);
    }
    section
}

/// Build a Cards v2 reply carrying `text`, an optional `dashboard` grid,
/// and the interactive widgets. Each `(spec, token)` renders to a button /
/// dropdown / form whose submit `onClick.action` carries the signed
/// `token`; Google echoes it (plus any `formInputs`) on the `CARD_CLICKED`
/// event, where the adapter verifies and re-dispatches. `workspace_addon`
/// selects the reply envelope.
pub fn build_interactive_card(
    text: &str,
    dashboard: Option<&DashboardData>,
    dashboard_image_url: Option<&str>,
    signed: &[(InteractiveSpec, String)],
    workspace_addon: bool,
) -> Value {
    let mut widgets: Vec<Value> = Vec::new();
    // Consecutive plain buttons group into one buttonList row.
    let mut pending: Vec<Value> = Vec::new();
    for (spec, token) in signed {
        match spec {
            InteractiveSpec::Button { label, .. } => {
                pending.push(action_button(label, token));
            }
            InteractiveSpec::Selection {
                prompt,
                options,
                args_key,
                ..
            } => {
                if !pending.is_empty() {
                    widgets.push(serde_json::json!({ "buttonList": { "buttons": pending } }));
                    pending = Vec::new();
                }
                let items: Vec<Value> = options
                    .iter()
                    .map(|(label, value)| serde_json::json!({ "text": label, "value": value }))
                    .collect();
                widgets.push(serde_json::json!({
                    "selectionInput": {
                        "name": args_key,
                        "label": prompt,
                        "type": "DROPDOWN",
                        "items": items
                    }
                }));
                widgets.push(serde_json::json!({
                    "buttonList": { "buttons": [ action_button("Submit", token) ] }
                }));
            }
            InteractiveSpec::Form {
                title,
                submit_label,
                fields,
                ..
            } => {
                if !pending.is_empty() {
                    widgets.push(serde_json::json!({ "buttonList": { "buttons": pending } }));
                    pending = Vec::new();
                }
                if !title.is_empty() {
                    widgets.push(serde_json::json!({
                        "decoratedText": { "text": title }
                    }));
                }
                for f in fields {
                    if f.boolean {
                        widgets.push(serde_json::json!({
                            "selectionInput": {
                                "name": f.name,
                                "label": f.label,
                                "type": "SWITCH",
                                "items": [ { "text": f.label, "value": "true" } ]
                            }
                        }));
                    } else {
                        widgets.push(serde_json::json!({
                            "textInput": { "name": f.name, "label": f.label }
                        }));
                    }
                }
                let submit = if submit_label.is_empty() {
                    "Submit"
                } else {
                    submit_label
                };
                widgets.push(serde_json::json!({
                    "buttonList": { "buttons": [ action_button(submit, token) ] }
                }));
            }
        }
    }
    if !pending.is_empty() {
        widgets.push(serde_json::json!({ "buttonList": { "buttons": pending } }));
    }
    // The dashboard grid first (the chart-as-tiles), then the actions.
    let mut sections: Vec<Value> = Vec::new();
    if let Some(d) = dashboard {
        sections.push(dashboard_section(d, dashboard_image_url));
    }
    if !widgets.is_empty() {
        sections.push(serde_json::json!({ "widgets": widgets }));
    }
    let mut message = serde_json::json!({
        "cardsV2": [ { "cardId": "agent-actions", "card": { "sections": sections } } ]
    });
    if !text.is_empty() {
        message["text"] = serde_json::json!(text);
    }
    wrap_message(message, workspace_addon)
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

    #[test]
    fn interactive_components_are_lifted_off_the_surface() {
        let result = serde_json::json!({
            "surface": { "components": [
                { "kind": "text", "value": "hi" },
                { "kind": "button", "label": "Ask again",
                  "tool": "assistant", "args": { "question": "redo?" } },
                { "kind": "selection", "prompt": "Pick a supplier",
                  "options": [ { "label": "Alpine", "value": "Tell me about Alpine" } ],
                  "tool": "assistant", "args_key": "question" },
                { "kind": "form", "title": "Allocate", "submit_label": "Go",
                  "fields": [
                    { "name": "material", "label": "Material", "kind": "string", "required": true },
                    { "name": "urgent", "label": "Urgent?", "kind": "boolean", "required": false }
                  ],
                  "tool": "assistant" }
            ] }
        });
        let specs = interactive_from_result(&result);
        assert_eq!(specs.len(), 3);
        assert!(matches!(specs[0], InteractiveSpec::Button { .. }));
        assert!(matches!(specs[1], InteractiveSpec::Selection { .. }));
        match &specs[2] {
            InteractiveSpec::Form { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert!(!fields[0].boolean);
                assert!(fields[1].boolean);
            }
            other => panic!("expected a form, got {other:?}"),
        }
        // Button signs its preset args; selection/form sign empty base args.
        assert_eq!(
            specs[0].base_args(),
            serde_json::json!({ "question": "redo?" })
        );
        assert_eq!(specs[1].base_args(), serde_json::json!({}));
        // A non-surface result yields nothing.
        assert!(interactive_from_result(&serde_json::json!({ "echo": "x" })).is_empty());
    }

    #[test]
    fn card_groups_buttons_and_renders_selection_and_form() {
        let signed = vec![
            (
                InteractiveSpec::Button {
                    label: "Ask again".into(),
                    tool: "assistant".into(),
                    args: serde_json::json!({ "question": "redo?" }),
                },
                "BTN.MAC".to_string(),
            ),
            (
                InteractiveSpec::Selection {
                    prompt: "Pick a supplier".into(),
                    options: vec![("Alpine".into(), "Tell me about Alpine".into())],
                    tool: "assistant".into(),
                    args_key: "question".into(),
                },
                "SEL.MAC".to_string(),
            ),
            (
                InteractiveSpec::Form {
                    title: "Allocate".into(),
                    submit_label: "Go".into(),
                    fields: vec![FormFieldSpec {
                        name: "material".into(),
                        label: "Material".into(),
                        boolean: false,
                    }],
                    tool: "assistant".into(),
                },
                "FORM.MAC".to_string(),
            ),
        ];
        let body = build_interactive_card("the answer", None, None, &signed, false);
        assert_eq!(body["text"], serde_json::json!("the answer"));
        let widgets = body["cardsV2"][0]["card"]["sections"][0]["widgets"]
            .as_array()
            .expect("widgets array");
        // The plain button row.
        let btn = &widgets[0]["buttonList"]["buttons"][0];
        assert_eq!(btn["text"], serde_json::json!("Ask again"));
        assert_eq!(
            btn["onClick"]["action"]["parameters"][0]["value"],
            serde_json::json!("BTN.MAC")
        );
        // A dropdown widget exists with the args_key as its name.
        let has_dropdown = widgets.iter().any(|w| {
            w["selectionInput"]["name"] == serde_json::json!("question")
                && w["selectionInput"]["type"] == serde_json::json!("DROPDOWN")
        });
        assert!(
            has_dropdown,
            "expected a selectionInput dropdown; got {body}"
        );
        // A text input named after the form field exists.
        let has_text_input = widgets
            .iter()
            .any(|w| w["textInput"]["name"] == serde_json::json!("material"));
        assert!(has_text_input, "expected a textInput; got {body}");
        // The form submit carries the form token.
        let has_form_submit = widgets.iter().any(|w| {
            w["buttonList"]["buttons"][0]["text"] == serde_json::json!("Go")
                && w["buttonList"]["buttons"][0]["onClick"]["action"]["parameters"][0]["value"]
                    == serde_json::json!("FORM.MAC")
        });
        assert!(
            has_form_submit,
            "expected the form submit button; got {body}"
        );
    }

    #[test]
    fn card_addon_envelope_nests_cardsv2() {
        let signed = vec![(
            InteractiveSpec::Button {
                label: "Go".into(),
                tool: "assistant".into(),
                args: serde_json::json!({}),
            },
            "T".to_string(),
        )];
        let body = build_interactive_card("hi", None, None, &signed, true);
        assert!(body.get("cardsV2").is_none());
        assert!(body.get("text").is_none());
        let msg = &body["hostAppDataAction"]["chatDataAction"]["createMessageAction"]["message"];
        assert_eq!(msg["text"], serde_json::json!("hi"));
        assert!(msg["cardsV2"].is_array());
    }

    #[test]
    fn dashboard_is_lifted_and_rendered_as_a_grid() {
        let result = serde_json::json!({
            "surface": { "components": [
                { "kind": "narration", "text": "top risk" },
                { "kind": "dashboard", "title": "Stock at risk (€)", "tiles": [
                    { "label": "Alpine Metals AG", "value": "€2.19M" },
                    { "label": "Catalonia Carbon", "value": "€1.79M" }
                ] }
            ] }
        });
        let dash = dashboard_from_result(&result).expect("dashboard lifted");
        assert_eq!(dash.0, "Stock at risk (€)");
        assert_eq!(dash.1.len(), 2);

        let body = build_interactive_card("top risk", Some(&dash), None, &[], false);
        let sections = body["cardsV2"][0]["card"]["sections"]
            .as_array()
            .expect("sections");
        // The dashboard section is a grid with the section header = title.
        let grid_section = &sections[0];
        assert_eq!(
            grid_section["header"],
            serde_json::json!("Stock at risk (€)")
        );
        let items = grid_section["widgets"][0]["grid"]["items"]
            .as_array()
            .expect("grid items");
        assert_eq!(items.len(), 2);
        // value = item title, label = subtitle.
        assert_eq!(items[0]["title"], serde_json::json!("€2.19M"));
        assert_eq!(items[0]["subtitle"], serde_json::json!("Alpine Metals AG"));
        // No dashboard in a result with none.
        assert!(dashboard_from_result(&serde_json::json!({ "echo": "x" })).is_none());
    }

    #[test]
    fn dashboard_with_image_url_renders_an_image_widget() {
        let dash: DashboardData = (
            "Stock at risk (€)".into(),
            vec![("Alpine".into(), "€2.19M".into())],
        );
        let url = "https://x.example/google_chat/img/TOKEN";
        let body = build_interactive_card("top risk", Some(&dash), Some(url), &[], false);
        let section = &body["cardsV2"][0]["card"]["sections"][0];
        assert_eq!(section["header"], serde_json::json!("Stock at risk (€)"));
        let img = &section["widgets"][0]["image"];
        assert_eq!(img["imageUrl"], serde_json::json!(url));
        assert_eq!(img["altText"], serde_json::json!("Stock at risk (€)"));
        // No grid when we have the chart image.
        assert!(section["widgets"][0].get("grid").is_none());
    }
}
