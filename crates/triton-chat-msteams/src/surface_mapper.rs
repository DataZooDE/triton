//! v0.2 — L6' surface mapper for the Microsoft Teams adapter.
//!
//! Bot Framework Activity replies are text-first; the connector's
//! published body cap is ~28 KB but we use Telegram's safer 4096-byte
//! ceiling so a single oversized tool output can never trip the
//! upstream length check. The plain-text path (Text + Narration)
//! projects onto the Activity `text` body.
//!
//! Issue #155 adds the **interactive** projection (architecture.md
//! §8.7 — the L6' AdaptiveCard pass): `Button` / `Selection` / `Form`
//! render as Adaptive Card actions/inputs and `Dashboard` renders as a
//! `FactSet`. Each interactive control's `(tool, base_args)` is signed
//! into an HMAC correlation token (via [`triton_correlation`], key held
//! by the adapter) carried in the action's `data.ct`; the callback
//! handler verifies it before re-dispatching, so a crafted Activity
//! can't drive an arbitrary tool.
//!
//! No HTML escaping: Teams renders Activity `text` as Markdown by
//! default, but for the conservative text slice we keep
//! `textFormat: plain` and pass the raw string through. Adaptive Card
//! `TextBlock`s render their content as-is.

use serde_json::{Value, json};
use triton_core::a2ui::{Component, FormFieldKind, Surface, extract_surface};

/// The Adaptive Card chrome (branded header) — fetched per reply from the
/// report upstream's `get_theme` tool. Peacock owns ALL theming (one CSS of
/// `--pk-*` tokens themes charts, iframes AND this chrome); this adapter
/// only consumes the resolved values and carries no theme config of its
/// own. Default = unbranded, i.e. the pre-theme rendering.
///
/// NB peacock's `brand_color` (`--pk-brand`) is deliberately NOT consumed
/// here, unlike the Google Chat chrome. Adaptive Cards carry no arbitrary
/// colour — `TextBlock.color` / `Container.style` / `Action.style` are all
/// host-resolved enums — and the only way to inject one is a served
/// background image. Teams renders light, dark AND high-contrast themes
/// that the card cannot detect, and nothing computes a contrasting
/// foreground for us the way Google Chat does for a `FILLED` button, so a
/// fixed brand band would be illegible in at least one theme. The header
/// uses the host-themed `emphasis` container instead. See
/// `doc/realizations.md` §7.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardChrome {
    /// Card header title (`--pk-name`).
    pub title: Option<String>,
    /// Card header logo — a public HTTPS image URL (`--pk-logo`).
    pub logo_url: Option<String>,
    /// How `logo_url` is placed (`--pk-logo-style`).
    pub logo_style: LogoStyle,
}

/// Placement of the chrome's `logo_url` on the rendered card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogoStyle {
    /// Small round image beside the title (square icon logos).
    #[default]
    Avatar,
    /// Full-width image above the title (wide wordmark logos).
    Banner,
}

impl CardChrome {
    /// Parse a `get_theme` tool result (`{title, logo_url, logo_style,
    /// brand_color, …}` — peacock's `mcp::get_theme`). `title` and
    /// `logo_url` are `Option` on the wire and arrive as `null` for an
    /// unbranded deployment; an unknown `logo_style` is the default —
    /// theming never fails a reply.
    pub fn from_get_theme(result: &Value) -> Self {
        let get = |k: &str| {
            result
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        CardChrome {
            title: get("title"),
            logo_url: get("logo_url"),
            logo_style: match result.get("logo_style").and_then(Value::as_str) {
                Some("banner") => LogoStyle::Banner,
                _ => LogoStyle::Avatar,
            },
        }
    }

    /// True when the theme has nothing to render — the card is then built
    /// exactly as it was before theming existed.
    fn is_unbranded(&self) -> bool {
        self.title.is_none() && self.logo_url.is_none()
    }
}

/// The branded header element prepended to a card's `body`, or `None`
/// when the theme sets neither a name nor a logo.
///
/// `emphasis` + `bleed` gives a full-width tinted band that the Teams
/// host themes for the viewer's light/dark/high-contrast mode (AC 1.2).
fn header_container(theme: &CardChrome) -> Option<Value> {
    if theme.is_unbranded() {
        return None;
    }
    let title = theme.title.as_deref();
    let logo = theme.logo_url.as_deref();
    let title_block = |t: &str| {
        json!({
            "type": "TextBlock", "text": t,
            "weight": "Bolder", "size": "Medium", "wrap": true
        })
    };
    let items: Vec<Value> = match (theme.logo_style, logo, title) {
        // Wide wordmark: full-width above the title, so the round avatar
        // slot can't crop it.
        (LogoStyle::Banner, Some(l), t) => {
            let mut v = vec![json!({
                "type": "Image", "url": l, "size": "Stretch",
                "altText": t.unwrap_or("logo"),
            })];
            v.extend(t.map(title_block));
            v
        }
        // Square icon beside the title.
        (LogoStyle::Avatar, Some(l), Some(t)) => vec![json!({
            "type": "ColumnSet",
            "columns": [
                { "type": "Column", "width": "auto", "items": [
                    { "type": "Image", "url": l, "size": "Small",
                      "style": "Person", "altText": t } ] },
                { "type": "Column", "width": "stretch",
                  "verticalContentAlignment": "Center",
                  "items": [ title_block(t) ] }
            ]
        })],
        (LogoStyle::Avatar, Some(l), None) => vec![json!({
            "type": "Image", "url": l, "size": "Small",
            "style": "Person", "altText": "logo",
        })],
        (_, None, Some(t)) => vec![title_block(t)],
        // Excluded by `is_unbranded` above.
        (_, None, None) => return None,
    };
    Some(json!({
        "type": "Container", "style": "emphasis", "bleed": true, "items": items
    }))
}

/// Hard ceiling on the Activity `text` body. The Bot Framework
/// documented limit is ~28 KB; we keep Telegram's 4096-byte safe
/// ceiling so chat-channel adapters share one truncation budget and
/// operator dashboards stay comparable.
pub const MSTEAMS_TEXT_MAX_BYTES: usize = 4096;

/// Sentinel appended when we truncate to fit
/// [`MSTEAMS_TEXT_MAX_BYTES`]. Square brackets make it visibly an
/// adapter artefact, not tool output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded the 4096-byte Activity cap]";

/// Rendered plain-text Teams Activity body. The interactive projection
/// builds Adaptive Card attachments separately (see
/// [`build_adaptive_card`]); this carries the text fallback.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

impl RenderedMessage {
    /// A bare-text message with no deferred-component counts — used for
    /// the plain-text fallback paths (empty surface, non-surface tool
    /// output).
    pub fn text_only(text: String) -> Self {
        RenderedMessage {
            text,
            deferred_buttons: 0,
            deferred_selections: 0,
            deferred_forms: 0,
            deferred_dashboards: 0,
            truncated: false,
        }
    }
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
            Component::Text { value, .. } => chunks.push(value.clone()),
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored by the text mapper.
            Component::Report { .. } => {}
            // Document references: an item whose `resource` is a real
            // https URL (the agent's signed /docs/{token} links, #635 P7)
            // renders as a markdown link — Teams renders markdown in both
            // bot messages and card TextBlocks. Anything else (ui://
            // internal refs) stays a plain label rather than a dead link.
            Component::Sources { items } => {
                if !items.is_empty() {
                    let rendered: Vec<String> = items
                        .iter()
                        .map(|i| {
                            if i.resource.starts_with("https://") {
                                format!("[{}]({})", i.label, i.resource)
                            } else {
                                i.label.clone()
                            }
                        })
                        .collect();
                    chunks.push(format!("Sources: {}", rendered.join(" \u{b7} ")));
                }
            }
            Component::Narration { text } => chunks.push(format!("_{}_", text)),
            // A fenced `diff` block: the +/- markers only line up in a
            // fixed-width font, and the fence is what gets one.
            Component::Diff { lines } => chunks.push(format!(
                "```diff\n{}\n```",
                triton_core::a2ui::diff_to_text(lines)
            )),
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

// ---- Interactive Adaptive Card projection (issue #155) -----------

/// Byte cap on an interactive control's signed correlation token.
/// Unlike Telegram's 64-byte `callback_data`, an Adaptive Card action
/// carries its token in a JSON `data` object with no tight per-field
/// budget (the card body cap is ~28 KB). We keep the same generous
/// cap Google Chat uses so a form/button with modest preset args never
/// trips the encoder; the decode side uses the matching cap so a token
/// minted for Teams can't be replayed through Telegram's 64-byte gate.
pub const MSTEAMS_CORRELATION_CAP: usize = 1536;

/// `verb` stamped on every `Action.Execute` (universal action) and
/// echoed back on the `adaptiveCard/action` invoke. Constant — the
/// callback only reads the signed token + any card inputs.
pub const ACTION_VERB: &str = "agentAction";

/// Key under which the signed correlation token rides in an action's
/// `data` object (and thus in the callback's `value`).
pub const TOKEN_DATA_KEY: &str = "ct";

/// The Adaptive Card attachment content type (Bot Framework).
pub const ADAPTIVE_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.card.adaptive";

const ADAPTIVE_CARD_SCHEMA: &str = "http://adaptivecards.io/schemas/adaptive-card.json";
/// Adaptive Card schema version. 1.4 is the floor that ships
/// `Action.Execute` (universal actions) across the Teams clients.
/// Not bumped past 1.4: the transpiler emits no property above AC 1.3
/// (`input_widget` uses only `label`/`isRequired`), and a host capped
/// below the declared version renders `fallbackText` instead of the
/// card — a downgrade on exactly the non-Teams surfaces this ingress
/// serves (triton#247 crew review F2).
const ADAPTIVE_CARD_VERSION: &str = "1.4";

/// One form field to render as an Adaptive Card `Input.*` widget.
#[derive(Debug, Clone)]
pub struct FormFieldSpec {
    pub name: String,
    pub label: String,
    pub kind: FormFieldKind,
    pub required: bool,
}

/// An interactive component lifted off a Surface. The adapter signs
/// each one's `(tool, base_args)` into a correlation token (it holds
/// the key; this stays key-free) and renders the matching Adaptive
/// Card widget(s) via [`build_adaptive_card`]. On the callback it
/// verifies the token and re-dispatches `tool` with `base_args` ⊕ the
/// user's card inputs.
#[derive(Debug, Clone)]
pub enum InteractiveSpec {
    /// One-tap preset → re-invoke `tool` with fixed `args`.
    Button {
        label: String,
        tool: String,
        args: Value,
    },
    /// Pick-one dropdown → re-invoke `tool` with the chosen value bound
    /// to `args_key` (delivered as a card input).
    Selection {
        prompt: String,
        options: Vec<(String, String)>,
        tool: String,
        args_key: String,
    },
    /// Multi-field form → re-invoke `tool` with each field's typed
    /// value (delivered as card inputs, keyed by field name).
    Form {
        title: String,
        submit_label: String,
        fields: Vec<FormFieldSpec>,
        tool: String,
    },
}

impl InteractiveSpec {
    /// The tool this control re-invokes.
    pub fn tool(&self) -> &str {
        match self {
            InteractiveSpec::Button { tool, .. }
            | InteractiveSpec::Selection { tool, .. }
            | InteractiveSpec::Form { tool, .. } => tool,
        }
    }

    /// The args signed into the correlation token. For a `Button` this
    /// is the full preset args; for `Selection`/`Form` it is empty —
    /// the user-supplied values arrive as card inputs on the callback
    /// and are merged then. Those values are query parameters, not a
    /// trust decision, so they need no signature; the token only fixes
    /// the **tool** (and any preset args) against tampering.
    pub fn base_args(&self) -> Value {
        match self {
            InteractiveSpec::Button { args, .. } => args.clone(),
            _ => json!({}),
        }
    }
}

/// Extract the interactive components (`Button` / `Selection` / `Form`)
/// from a dispatch result's A2UI surface, in order. Empty when the
/// result isn't a surface or has none (caller then sends a plain-text
/// reply).
pub fn interactive_from_result(result: &Value) -> Vec<InteractiveSpec> {
    let Ok(surface) = extract_surface(result) else {
        return Vec::new();
    };
    surface
        .components
        .iter()
        .filter_map(|c| match c {
            Component::Button {
                label, tool, args, ..
            } => Some(InteractiveSpec::Button {
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
                        kind: f.kind,
                        required: f.required,
                    })
                    .collect(),
                tool: tool.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// A `Dashboard` lifted off a surface for Adaptive Card rendering: its
/// title and `(label, value)` metric tiles.
pub type DashboardData = (String, Vec<(String, String)>);

/// The inline `Report` component the agent asked to embed, when the
/// surface carries one — `(report_id, args)`. The msteams reply path
/// dispatches `render_report` and shows the chart PNG in the card via
/// the signed image route (#635; port of the googlechat expansion).
pub fn report_from_result(result: &Value) -> Option<(String, Value)> {
    let surface = extract_surface(result).ok()?;
    surface.components.iter().find_map(|c| match c {
        Component::Report {
            report_id, args, ..
        } => Some((report_id.clone(), args.clone())),
        _ => None,
    })
}

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

/// One `Action.Execute` (universal action) carrying the signed
/// correlation `token`. Universal actions let the bot return a
/// refreshed card in place; Teams clients that don't support them
/// transparently downgrade to `Action.Submit`, which the callback
/// handler also accepts.
fn execute_action(title: &str, token: &str) -> Value {
    json!({
        "type": "Action.Execute",
        "verb": ACTION_VERB,
        "title": title,
        "data": { TOKEN_DATA_KEY: token },
    })
}

/// One Adaptive Card `Input.*` widget for a form field.
fn input_widget(field: &FormFieldSpec) -> Value {
    match field.kind {
        FormFieldKind::Boolean => json!({
            "type": "Input.Toggle",
            "id": field.name,
            "title": field.label,
        }),
        FormFieldKind::Integer => json!({
            "type": "Input.Number",
            "id": field.name,
            "label": field.label,
            "isRequired": field.required,
        }),
        FormFieldKind::String => json!({
            "type": "Input.Text",
            "id": field.name,
            "label": field.label,
            "isRequired": field.required,
        }),
    }
}

/// Build the Adaptive Card `content` object from the rendered `text`,
/// an optional `dashboard` FactSet, and the signed interactive specs.
///
/// Plain `Button`s land in the card's top-level `actions` bar;
/// `Selection`/`Form` render their inputs inline in the body followed
/// by an `ActionSet` submit — Teams gathers every input on the card
/// with any action, so the callback merges the (non-empty) inputs onto
/// the token's signed base args.
///
/// `chrome` is peacock's resolved theme (`get_theme`); a branded one
/// leads the body with a header band. [`CardChrome::default`] renders
/// byte-identically to the pre-theming card.
pub fn build_adaptive_card(
    text: &str,
    dashboard: Option<&DashboardData>,
    signed: &[(InteractiveSpec, String)],
    image_url: Option<&str>,
    chrome: &CardChrome,
) -> Value {
    let mut body: Vec<Value> = Vec::new();
    body.extend(header_container(chrome));
    if !text.is_empty() {
        body.push(json!({ "type": "TextBlock", "text": text, "wrap": true }));
    }
    // The rendered chart (inline report / upstream PNG), served from the
    // adapter's signed image route — Teams fetches it by URL (#635).
    if let Some(url) = image_url {
        body.push(json!({ "type": "Image", "url": url, "altText": "chart" }));
    }
    if let Some((title, tiles)) = dashboard {
        if !title.is_empty() {
            body.push(json!({
                "type": "TextBlock", "text": title, "weight": "Bolder", "wrap": true
            }));
        }
        let facts: Vec<Value> = tiles
            .iter()
            .map(|(label, value)| json!({ "title": label, "value": value }))
            .collect();
        body.push(json!({ "type": "FactSet", "facts": facts }));
    }

    let mut actions: Vec<Value> = Vec::new();
    for (spec, token) in signed {
        match spec {
            InteractiveSpec::Button { label, .. } => {
                actions.push(execute_action(label, token));
            }
            InteractiveSpec::Selection {
                prompt,
                options,
                args_key,
                ..
            } => {
                let choices: Vec<Value> = options
                    .iter()
                    .map(|(label, value)| json!({ "title": label, "value": value }))
                    .collect();
                body.push(json!({
                    "type": "Input.ChoiceSet",
                    "id": args_key,
                    "label": prompt,
                    "style": "compact",
                    "choices": choices,
                }));
                body.push(json!({
                    "type": "ActionSet",
                    "actions": [ execute_action("Submit", token) ],
                }));
            }
            InteractiveSpec::Form {
                title,
                submit_label,
                fields,
                ..
            } => {
                if !title.is_empty() {
                    body.push(json!({
                        "type": "TextBlock", "text": title, "weight": "Bolder", "wrap": true
                    }));
                }
                for f in fields {
                    body.push(input_widget(f));
                }
                let submit = if submit_label.is_empty() {
                    "Submit"
                } else {
                    submit_label
                };
                body.push(json!({
                    "type": "ActionSet",
                    "actions": [ execute_action(submit, token) ],
                }));
            }
        }
    }

    let mut card = json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
    });
    if !actions.is_empty() {
        card["actions"] = Value::Array(actions);
    }
    card
}

/// Wrap an Adaptive Card `content` object in a Bot Framework reply
/// Activity for the conversation the inbound arrived on. `bot_id`
/// mirrors the inbound's `recipient.id` (the bot); `recipient_id`
/// mirrors the inbound's `from.id` (the user we're replying to).
pub fn build_card_activity_body(
    bot_id: &str,
    conversation_id: &str,
    recipient_id: &str,
    card: Value,
) -> Value {
    json!({
        "type": "message",
        "from": { "id": bot_id },
        "conversation": { "id": conversation_id },
        "recipient": { "id": recipient_id },
        "attachments": [ {
            "contentType": ADAPTIVE_CARD_CONTENT_TYPE,
            "content": card,
        } ],
    })
}

/// The `invoke` HTTP response body that returns a refreshed Adaptive
/// Card in place (the `Action.Execute` drill-down). Teams renders
/// `value` as the new card, replacing the tapped one.
pub fn invoke_card_response(card: Value) -> Value {
    json!({
        "statusCode": 200,
        "type": ADAPTIVE_CARD_CONTENT_TYPE,
        "value": card,
    })
}

/// The `invoke` HTTP response body for the no-card case — a plain
/// message the client shows without an in-place card refresh.
pub fn invoke_message_response(text: &str) -> Value {
    json!({
        "statusCode": 200,
        "type": "application/vnd.microsoft.activity.message",
        "value": text,
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
                    pills: Default::default(),
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
            components: vec![Component::Text {
                pills: Default::default(),
                value: big,
            }],
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
                Component::Text {
                    pills: Default::default(),
                    value: "x".into(),
                },
                Component::Button {
                    primary: false,
                    label: "Click".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                    resource: None,
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.deferred_buttons, 1);
    }

    #[test]
    fn interactive_components_are_lifted_off_the_surface() {
        let result = json!({
            "surface": { "components": [
                { "kind": "text", "value": "hi" },
                { "kind": "button", "label": "Refresh",
                  "tool": "narrate", "args": { "subject": "alice" } },
                { "kind": "selection", "prompt": "Pick a supplier",
                  "options": [ { "label": "Alpine", "value": "Tell me about Alpine" } ],
                  "tool": "assistant", "args_key": "question" },
                { "kind": "form", "title": "Allocate", "submit_label": "Go",
                  "fields": [
                    { "name": "material", "label": "Material", "kind": "string", "required": true },
                    { "name": "qty", "label": "Quantity", "kind": "integer", "required": false },
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
                assert_eq!(fields.len(), 3);
                assert!(matches!(fields[0].kind, FormFieldKind::String));
                assert!(matches!(fields[1].kind, FormFieldKind::Integer));
                assert!(matches!(fields[2].kind, FormFieldKind::Boolean));
            }
            other => panic!("expected a form, got {other:?}"),
        }
        // Button signs its preset args; selection/form sign empty base args.
        assert_eq!(specs[0].base_args(), json!({ "subject": "alice" }));
        assert_eq!(specs[1].base_args(), json!({}));
        // A non-surface result yields nothing.
        assert!(interactive_from_result(&json!({ "echo": "x" })).is_empty());
    }

    #[test]
    fn card_renders_button_as_top_level_execute_action() {
        let signed = vec![(
            InteractiveSpec::Button {
                label: "Refresh".into(),
                tool: "narrate".into(),
                args: json!({ "subject": "alice" }),
            },
            "TOKEN.MAC".to_string(),
        )];
        let card =
            build_adaptive_card("Hello, alice.", None, &signed, None, &CardChrome::default());
        assert_eq!(card["type"], "AdaptiveCard");
        // Text lands in a body TextBlock.
        assert_eq!(card["body"][0]["type"], "TextBlock");
        assert_eq!(card["body"][0]["text"], "Hello, alice.");
        // The button is a top-level Action.Execute carrying the token.
        let action = &card["actions"][0];
        assert_eq!(action["type"], "Action.Execute");
        assert_eq!(action["verb"], ACTION_VERB);
        assert_eq!(action["title"], "Refresh");
        assert_eq!(action["data"][TOKEN_DATA_KEY], "TOKEN.MAC");
    }

    #[test]
    fn card_renders_selection_and_form_inputs_with_submit_actions() {
        let signed = vec![
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
                    fields: vec![
                        FormFieldSpec {
                            name: "material".into(),
                            label: "Material".into(),
                            kind: FormFieldKind::String,
                            required: true,
                        },
                        FormFieldSpec {
                            name: "urgent".into(),
                            label: "Urgent?".into(),
                            kind: FormFieldKind::Boolean,
                            required: false,
                        },
                    ],
                    tool: "assistant".into(),
                },
                "FORM.MAC".to_string(),
            ),
        ];
        let card = build_adaptive_card("", None, &signed, None, &CardChrome::default());
        let body = card["body"].as_array().expect("body array");
        // Dropdown named after args_key.
        let choiceset = body
            .iter()
            .find(|w| w["type"] == "Input.ChoiceSet")
            .expect("choiceset");
        assert_eq!(choiceset["id"], "question");
        assert_eq!(choiceset["choices"][0]["value"], "Tell me about Alpine");
        // Text + toggle inputs named after the form fields.
        assert!(
            body.iter()
                .any(|w| w["type"] == "Input.Text" && w["id"] == "material")
        );
        assert!(
            body.iter()
                .any(|w| w["type"] == "Input.Toggle" && w["id"] == "urgent")
        );
        // Two ActionSet submits carrying the two tokens.
        let submit_tokens: Vec<&str> = body
            .iter()
            .filter(|w| w["type"] == "ActionSet")
            .filter_map(|w| w["actions"][0]["data"][TOKEN_DATA_KEY].as_str())
            .collect();
        assert!(submit_tokens.contains(&"SEL.MAC"));
        assert!(submit_tokens.contains(&"FORM.MAC"));
        // No top-level actions (only submits, which live in the body).
        assert!(card.get("actions").is_none());
    }

    #[test]
    fn dashboard_renders_as_a_factset() {
        let result = json!({
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
        let card = build_adaptive_card("top risk", Some(&dash), &[], None, &CardChrome::default());
        let body = card["body"].as_array().expect("body");
        let factset = body
            .iter()
            .find(|w| w["type"] == "FactSet")
            .expect("factset");
        let facts = factset["facts"].as_array().unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0]["title"], "Alpine Metals AG");
        assert_eq!(facts[0]["value"], "€2.19M");
    }

    #[test]
    fn card_activity_and_invoke_response_shapes() {
        let card = build_adaptive_card("hi", None, &[], None, &CardChrome::default());
        let activity = build_card_activity_body("28:bot", "a:conv", "29:user", card.clone());
        assert_eq!(activity["type"], "message");
        assert_eq!(
            activity["attachments"][0]["contentType"],
            ADAPTIVE_CARD_CONTENT_TYPE
        );
        assert_eq!(
            activity["attachments"][0]["content"]["type"],
            "AdaptiveCard"
        );
        assert_eq!(activity["from"]["id"], "28:bot");
        assert_eq!(activity["recipient"]["id"], "29:user");

        let inv = invoke_card_response(card);
        assert_eq!(inv["statusCode"], 200);
        assert_eq!(inv["type"], ADAPTIVE_CARD_CONTENT_TYPE);
        assert_eq!(inv["value"]["type"], "AdaptiveCard");
    }

    /// #635 P7: a Sources item with a real https resource renders as a
    /// markdown link; a ui:// internal ref stays a plain label — never
    /// a dead link.
    #[test]
    fn sources_render_https_resources_as_links() {
        let s = Surface {
            components: vec![Component::Sources {
                items: vec![
                    triton_core::a2ui::SourceItem {
                        label: "verify-567-memo".into(),
                        resource: "https://agent.example/docs/tok123".into(),
                    },
                    triton_core::a2ui::SourceItem {
                        label: "internal-page".into(),
                        resource: "ui://peacock/document?id=x".into(),
                    },
                ],
            }],
        };
        let out = render(&s).expect("renders");
        assert!(
            out.text
                .contains("[verify-567-memo](https://agent.example/docs/tok123)"),
            "{}",
            out.text
        );
        assert!(out.text.contains("internal-page"), "{}", out.text);
        assert!(
            !out.text.contains("ui://"),
            "internal refs must not leak as fake links: {}",
            out.text
        );
    }

    /// #635: an image URL renders as a card Image element; absent, the
    /// card body is unchanged.
    #[test]
    fn card_includes_image_when_url_present() {
        let card = build_adaptive_card(
            "with chart",
            None,
            &[],
            Some("https://x/img/t"),
            &CardChrome::default(),
        );
        let imgs: Vec<_> = card["body"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "Image")
            .collect();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0]["url"], "https://x/img/t");
    }

    /// The inline Report component lifts to `(report_id, args)` for the
    /// render_report expansion — port of the googlechat behaviour.
    #[test]
    fn report_lifts_from_surface() {
        let result = json!({ "surface": { "components": [
            { "kind": "report", "report_id": "top-categories", "args": { "params": {} } }
        ] } });
        let (id, args) = report_from_result(&result).expect("report lifted");
        assert_eq!(id, "top-categories");
        assert!(args.is_object());
    }

    // ---- #200: peacock get_theme chrome -----------------------------

    /// A theme with neither `--pk-name` nor `--pk-logo` — peacock's
    /// unbranded default, where both arrive as JSON `null` — must leave
    /// the card exactly as it was before theming existed.
    #[test]
    fn unbranded_theme_adds_no_header() {
        let chrome = CardChrome::from_get_theme(&json!({
            "brand": "-", "host": "teams.example",
            "title": null, "logo_url": null, "logo_style": "avatar",
            // peacock ALWAYS resolves a brand colour (stock default when
            // no brand CSS is registered) — so a colour is never the
            // signal that a deployment is branded.
            "brand_color": "#0f6cbd", "accent": "#22d3c5", "css": ":root {}",
        }));
        assert_eq!(chrome, CardChrome::default());
        let card = build_adaptive_card("hi", None, &[], None, &chrome);
        assert_eq!(card["body"][0]["type"], "TextBlock");
        assert_eq!(card["body"][0]["text"], "hi");
    }

    /// Junk in `--pk-logo-style` falls back to `avatar`, and empty
    /// strings read as unset — theming never fails a reply.
    #[test]
    fn theme_parse_is_lenient() {
        let chrome = CardChrome::from_get_theme(&json!({
            "title": "", "logo_url": "https://x/l.png", "logo_style": "hexagon",
        }));
        assert_eq!(chrome.title, None, "empty string reads as unset");
        assert_eq!(chrome.logo_style, LogoStyle::Avatar, "junk style defaults");
        // A logo with no name still brands: the icon alone, no title row.
        let card = build_adaptive_card("hi", None, &[], None, &chrome);
        let header = &card["body"][0];
        assert_eq!(header["type"], "Container");
        assert_eq!(header["items"][0]["type"], "Image");
        assert_eq!(header["items"][0]["style"], "Person");
    }

    /// `--pk-name` with no logo renders the title alone in the band.
    #[test]
    fn title_only_theme_renders_a_text_header() {
        let chrome = CardChrome::from_get_theme(&json!({ "title": "DataZoo Sales" }));
        let card = build_adaptive_card("hi", None, &[], None, &chrome);
        let header = &card["body"][0];
        assert_eq!(header["type"], "Container");
        assert_eq!(header["style"], "emphasis");
        assert_eq!(header["bleed"], true);
        assert_eq!(header["items"][0]["type"], "TextBlock");
        assert_eq!(header["items"][0]["text"], "DataZoo Sales");
        // The answer follows the header, it is not replaced by it.
        assert_eq!(card["body"][1]["text"], "hi");
    }

    /// The header precedes the chart image, so a themed report card
    /// reads title → chart rather than chart → title.
    #[test]
    fn header_precedes_the_chart_image() {
        let chrome = CardChrome::from_get_theme(&json!({
            "title": "DataZoo Sales", "logo_style": "banner",
        }));
        let card = build_adaptive_card("", None, &[], Some("https://x/img/t"), &chrome);
        assert_eq!(card["body"][0]["type"], "Container");
        assert_eq!(card["body"][1]["type"], "Image");
        assert_eq!(card["body"][1]["url"], "https://x/img/t");
    }
}
