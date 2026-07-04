//! L6' surface mapper for WhatsApp Cloud API.
//!
//! Takes the canonical `triton_core::a2ui::Surface` and renders it
//! into a WhatsApp `messages` body.
//!
//! Output discipline:
//!   * text → plain text (WhatsApp doesn't speak HTML/Markdown in
//!     the same parse-mode way Telegram does).
//!   * narration → `_<text>_` (WhatsApp's documented italic markup;
//!     applied client-side only, no risk of breaking parsing if it
//!     fails to render).
//!   * buttons → Cloud API `interactive` reply buttons (#94); the
//!     first selection → an `interactive` list. Each `id` is a signed
//!     correlation token, never the tool/args. Overflow + forms +
//!     additional selections defer (counted in `deferred_*`).
//!   * dashboard → surfaced for rasterisation (PR 38). A dashboard in
//!     the surface takes the image route and defers interactive
//!     primitives (an image message carries no action).
//!
//! Truncation strategy mirrors Telegram's: cut between components
//! first, raw-text-budget fallback for a single oversized chunk.
//! The cap is WhatsApp's documented 4096 chars; we treat it as
//! bytes (any UTF-8 string ≤ 4096 bytes is ≤ 4096 UTF-16 code
//! units, so a byte cap only ever rejects messages WhatsApp itself
//! would also reject).

use serde_json::{Value, json};
use triton_core::a2ui::{Component, DashboardTile, Surface, extract_surface};

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
    /// Number of `Component::Dashboard` entries we couldn't ship.
    /// PR 38 wires the rasterizer, so the FIRST Dashboard in a
    /// surface is surfaced via [`Self::dashboard`] for the adapter
    /// to rasterise + send as a WhatsApp image message. Any
    /// subsequent dashboards bump this counter — WhatsApp image
    /// messages carry one media attachment.
    pub deferred_dashboards: usize,
    pub truncated: bool,
    /// PR 38: the FIRST `Component::Dashboard` found in the surface,
    /// surfaced to the caller for rasterisation. The adapter calls
    /// the rasterizer service, uploads the PNG to WhatsApp's media
    /// endpoint, and dispatches an `image` message carrying the
    /// returned `media_id`. Mirrors Telegram PR 36's
    /// `RenderedMessage::dashboard` slot.
    pub dashboard: Option<RasterDashboard>,
    /// #94: a prebuilt Cloud API `interactive` object (reply `button`
    /// or `list`) rendered from `Button` / `Selection` components. When
    /// `Some`, the adapter posts a `type: interactive` message instead
    /// of plain text; [`Self::text`] holds the same `body.text` for
    /// preview parity. Built only when no dashboard is present (an
    /// interactive message carries no media).
    pub interactive: Option<Value>,
}

/// WhatsApp Cloud API interactive limits (Meta-documented).
const MAX_REPLY_BUTTONS: usize = 3;
const MAX_LIST_ROWS: usize = 10;
const BUTTON_TITLE_MAX: usize = 20;
const LIST_ROW_TITLE_MAX: usize = 24;
const LIST_MENU_LABEL_MAX: usize = 20;
const INTERACTIVE_BODY_MAX: usize = 1024;

/// Dashboard payload the caller passes to the rasterizer. Local to
/// the adapter — each chat adapter owns its own L6′ rendering
/// concerns; sharing the type across crates would couple them.
#[derive(Debug, Clone)]
pub struct RasterDashboard {
    pub title: String,
    pub tiles: Vec<DashboardTile>,
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
pub fn try_render_surface(
    result: &Value,
    correlation_key: &[u8],
) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface, correlation_key))
}

/// Render a [`Surface`] into a `RenderedMessage` or a
/// `RenderError`. Public so the in-crate unit tests can exercise
/// the mapper without spinning the whole binary. `correlation_key`
/// signs the interactive `id`s (#94) so a future inbound handler can
/// route a tap back to its `(tool, args)`.
pub fn render(surface: &Surface, correlation_key: &[u8]) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    // #94: interactive primitives collected in source order. `Button`s
    // become reply buttons; the FIRST `Selection` becomes a list. Each
    // id is a signed correlation token — the tool/args MUST NOT leak as
    // plain text the user could retype.
    let mut buttons: Vec<InteractiveChoice> = Vec::new();
    let mut selection: Option<InteractiveSelection> = None;
    // PR 38: first Dashboard surfaced to the caller for rasterisation.
    // WhatsApp image messages carry one media attachment, so any
    // subsequent dashboards bump `deferred_dashboards`.
    let mut first_dashboard: Option<RasterDashboard> = None;
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
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored by the text mapper.
            Component::Report { .. } => {}
            Component::Narration { text } => {
                chunks.push(format!("_{text}_"));
                raw_sources.push(Some(RawChunk {
                    kind: RawKind::Narration,
                    raw: text.clone(),
                }));
            }
            Component::Button {
                label, tool, args, ..
            } => {
                // #94: render as a Cloud API reply button. The id is a
                // signed correlation token (NOT the tool/args), so the
                // user can't re-execute by typing. Defer on encode
                // failure (token over cap).
                match triton_correlation::encode(tool, args, correlation_key) {
                    Ok(token) => buttons.push(InteractiveChoice {
                        title: truncate_chars(label, BUTTON_TITLE_MAX),
                        id: token,
                    }),
                    Err(_) => deferred_buttons += 1,
                }
            }
            Component::Selection {
                prompt,
                options,
                tool,
                args_key,
            } => {
                // #94: the FIRST Selection becomes a Cloud API list;
                // additional selections defer (one list per message).
                if selection.is_some() {
                    deferred_selections += 1;
                    continue;
                }
                let mut rows = Vec::new();
                for opt in options {
                    let args = json!({ args_key.as_str(): &opt.value });
                    // Skip individual over-cap options; the rest of the
                    // list still ships.
                    if let Ok(token) = triton_correlation::encode(tool, &args, correlation_key) {
                        rows.push(InteractiveChoice {
                            title: truncate_chars(&opt.label, LIST_ROW_TITLE_MAX),
                            id: token,
                        });
                    }
                }
                if rows.is_empty() {
                    deferred_selections += 1;
                } else {
                    selection = Some(InteractiveSelection {
                        prompt: prompt.clone(),
                        rows,
                    });
                }
            }
            Component::Form { .. } => {
                deferred_forms += 1;
            }
            Component::Dashboard { title, tiles } => {
                // PR 38: the manifest's `dashboard: rasterised_png`
                // degrade rule now has a real renderer behind it.
                // The mapper surfaces the FIRST Dashboard for
                // rasterisation. The adapter uploads the PNG to
                // WhatsApp's media endpoint and dispatches an
                // `image` message carrying the returned media_id.
                //
                // We deliberately do NOT push a text chunk for the
                // dashboard: surrounding Text/Narration components
                // become the IMAGE's caption (WhatsApp's image
                // message supports an inline `caption` field), and
                // the dashboard itself is the image. The fallback
                // path adds its own placeholder string when needed.
                if first_dashboard.is_none() {
                    first_dashboard = Some(RasterDashboard {
                        title: title.clone(),
                        tiles: tiles.clone(),
                    });
                } else {
                    deferred_dashboards += 1;
                }
            }
        }
    }

    // #94: render interactive primitives. A Selection wins (renders as
    // a list, deferring any buttons); otherwise Buttons render as reply
    // buttons (capped at 3, overflow deferred). Skipped when a dashboard
    // is present — that message ships as an image, which carries no
    // interactive action.
    let interactive = if first_dashboard.is_some() {
        deferred_buttons += buttons.len();
        if selection.is_some() {
            deferred_selections += 1;
        }
        None
    } else if let Some(sel) = selection.take() {
        deferred_buttons += buttons.len();
        Some(build_list_interactive(
            &interactive_body_text(&chunks),
            &sel,
        ))
    } else if !buttons.is_empty() {
        let overflow = buttons.len().saturating_sub(MAX_REPLY_BUTTONS);
        deferred_buttons += overflow;
        buttons.truncate(MAX_REPLY_BUTTONS);
        Some(build_button_interactive(
            &interactive_body_text(&chunks),
            &buttons,
        ))
    } else {
        None
    };
    if let Some(interactive) = interactive {
        return Ok(RenderedMessage {
            text: interactive_body_text(&chunks),
            deferred_buttons,
            deferred_selections,
            deferred_forms,
            deferred_dashboards,
            truncated: false,
            dashboard: None,
            interactive: Some(interactive),
        });
    }

    // PR 38: a dashboard-only surface is legitimate — WhatsApp's
    // image messages don't require a caption, so we can ship a
    // photo with no text. Only refuse when there's literally
    // nothing renderable.
    if chunks.is_empty() && first_dashboard.is_none() {
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
            dashboard: first_dashboard,
            interactive: None,
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
            dashboard: first_dashboard,
            interactive: None,
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
        dashboard: first_dashboard,
        interactive: None,
    })
}

/// One reply button / list row: a display title + the signed
/// correlation token that goes in the Cloud API `id`.
struct InteractiveChoice {
    title: String,
    id: String,
}

/// The first `Selection` collected from a surface, ready to render as a
/// Cloud API list.
struct InteractiveSelection {
    prompt: String,
    rows: Vec<InteractiveChoice>,
}

/// Mandatory `interactive.body.text` — joined chunks, capped, with a
/// non-empty fallback (Cloud API rejects an empty interactive body).
fn interactive_body_text(chunks: &[String]) -> String {
    let joined = chunks.join("\n\n");
    let joined = if joined.is_empty() {
        "Please choose:".to_string()
    } else {
        joined
    };
    budget_raw(&joined, INTERACTIVE_BODY_MAX).to_string()
}

/// Build a Cloud API `interactive` reply-button object.
fn build_button_interactive(body_text: &str, buttons: &[InteractiveChoice]) -> Value {
    let buttons: Vec<Value> = buttons
        .iter()
        .map(|b| json!({ "type": "reply", "reply": { "id": b.id, "title": b.title } }))
        .collect();
    json!({
        "type": "button",
        "body": { "text": body_text },
        "action": { "buttons": buttons },
    })
}

/// Build a Cloud API `interactive` list object.
fn build_list_interactive(body_text: &str, sel: &InteractiveSelection) -> Value {
    let menu = if sel.prompt.is_empty() {
        "Choose".to_string()
    } else {
        truncate_chars(&sel.prompt, LIST_MENU_LABEL_MAX)
    };
    let rows: Vec<Value> = sel
        .rows
        .iter()
        .take(MAX_LIST_ROWS)
        .map(|r| json!({ "id": r.id, "title": r.title }))
        .collect();
    json!({
        "type": "list",
        "body": { "text": body_text },
        "action": {
            "button": menu,
            "sections": [ { "title": "Options", "rows": rows } ],
        },
    })
}

/// Build the WhatsApp `messages` body for a prebuilt `interactive`
/// object (#94).
pub fn build_interactive_body(to: &str, interactive: &Value) -> Value {
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "interactive",
        "interactive": interactive,
    })
}

/// Truncate `s` to at most `max` characters (not bytes), on a char
/// boundary — WhatsApp counts interactive titles in characters.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Build the WhatsApp `messages` body for an `image` message
/// carrying a previously-uploaded media_id. The PR 38 dashboard
/// path uploads the rasterised PNG to `/v18.0/{id}/media` first,
/// then sends this body to `/v18.0/{id}/messages`. The optional
/// caption is the rendered Text/Narration content that surrounded
/// the dashboard in the source surface.
pub fn build_image_message_body(to: &str, media_id: &str, caption: Option<&str>) -> Value {
    let mut image = serde_json::Map::new();
    image.insert("id".into(), Value::String(media_id.to_string()));
    if let Some(c) = caption.filter(|s| !s.is_empty()) {
        image.insert("caption".into(), Value::String(c.to_string()));
    }
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "image",
        "image": Value::Object(image),
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

/// Build a WhatsApp Cloud API `type: template` body (#94). Used for
/// proactive sends outside the 24-hour service window, where free-form
/// text is rejected by Meta. `name`/`language` come from the manifest's
/// `templates` map (Triton owns template selection); `variables` are the
/// ordered body parameters the agent supplied, substituted into the
/// template's `{{1}}, {{2}}, …` placeholders. An empty `variables` omits
/// the `components` array (templates with no body parameters).
pub fn build_template_body(to: &str, name: &str, language: &str, variables: &[String]) -> Value {
    let mut template = serde_json::Map::new();
    template.insert("name".into(), Value::String(name.to_string()));
    template.insert("language".into(), json!({ "code": language }));
    if !variables.is_empty() {
        let parameters: Vec<Value> = variables
            .iter()
            .map(|v| json!({ "type": "text", "text": v }))
            .collect();
        template.insert(
            "components".into(),
            json!([{ "type": "body", "parameters": parameters }]),
        );
    }
    json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "template",
        "template": Value::Object(template),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic correlation key for the unit tests.
    const KEY: [u8; 32] = [7u8; 32];
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
        let r = render(&s, &KEY).expect("renders");
        assert_eq!(r.text, "hello\n\n_a footnote_");
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn buttons_selection_form_are_deferred_dashboard_is_surfaced() {
        // PR 38 update: Dashboards are no longer deferred — the
        // first one is surfaced to the caller for rasterisation
        // (subsequent ones bump `deferred_dashboards`). The other
        // interactive primitives remain deferred until PR 32's
        // numbered_prompts wiring lands on WhatsApp.
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
                    resource: None,
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
        let r = render(&s, &KEY).expect("renders");
        // None of the interactive components contribute to text.
        assert_eq!(r.text, "preamble");
        assert_eq!(r.deferred_buttons, 1);
        assert_eq!(r.deferred_selections, 1);
        assert_eq!(r.deferred_forms, 1);
        // PR 38: first dashboard surfaced, not deferred.
        assert_eq!(r.deferred_dashboards, 0);
        let dash = r.dashboard.as_ref().expect("dashboard surfaced");
        assert_eq!(dash.title, "Secrets");
        assert_eq!(dash.tiles[0].label, "invocations");
        // Dashboard tile content MUST NEVER leak into the body
        // (would silently violate the `rasterised_png` degrade rule).
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
    }

    #[test]
    fn second_dashboard_is_deferred_first_is_surfaced() {
        // WhatsApp image messages carry one media attachment. With
        // two Dashboards in the same surface, the first becomes the
        // image and any others bump `deferred_dashboards`.
        use triton_core::a2ui::DashboardTile;
        let make_dash = |title: &str| Component::Dashboard {
            title: title.into(),
            tiles: vec![DashboardTile {
                label: "x".into(),
                value: "1".into(),
                trend: None,
            }],
        };
        let s = Surface {
            components: vec![make_dash("first"), make_dash("second")],
        };
        let r = render(&s, &KEY).expect("renders");
        let dash = r.dashboard.expect("first surfaced");
        assert_eq!(dash.title, "first");
        assert_eq!(r.deferred_dashboards, 1);
    }

    #[test]
    fn dashboard_only_surface_renders_with_empty_caption() {
        // A surface that's nothing but a Dashboard is now valid:
        // WhatsApp's image message accepts no caption, so we ship
        // the photo with no text rather than failing
        // EmptyAfterRender.
        use triton_core::a2ui::DashboardTile;
        let s = Surface {
            components: vec![Component::Dashboard {
                title: "Solo".into(),
                tiles: vec![DashboardTile {
                    label: "x".into(),
                    value: "1".into(),
                    trend: None,
                }],
            }],
        };
        let r = render(&s, &KEY).expect("renders");
        assert!(r.text.is_empty());
        assert!(r.dashboard.is_some());
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(
            render(&s, &KEY),
            Err(RenderError::EmptyAfterRender)
        ));
    }

    #[test]
    fn button_only_surface_renders_interactive_with_fallback_body() {
        // #94: a button-only surface now renders an interactive reply
        // button. Cloud API requires a non-empty body, so a synthetic
        // fallback is used. The tool/args MUST NOT leak as text.
        use serde_json::json;
        let s = Surface {
            components: vec![Component::Button {
                label: "Click".into(),
                tool: "narrate".into(),
                args: json!({}),
                resource: None,
            }],
        };
        let r = render(&s, &KEY).expect("renders");
        let i = r.interactive.expect("interactive built");
        assert_eq!(i["type"], "button");
        assert!(i["body"]["text"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(i["action"]["buttons"][0]["reply"]["title"], "Click");
        assert!(!serde_json::to_string(&i).unwrap().contains("narrate"));
    }

    #[test]
    fn buttons_over_cap_defer_the_overflow() {
        // WhatsApp reply-button messages carry at most 3 buttons.
        use serde_json::json;
        let button = |label: &str| Component::Button {
            label: label.into(),
            tool: "narrate".into(),
            args: json!({ "subject": label }),
            resource: None,
        };
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "pick".into(),
                },
                button("a"),
                button("b"),
                button("c"),
                button("d"),
            ],
        };
        let r = render(&s, &KEY).expect("renders");
        let i = r.interactive.expect("interactive built");
        assert_eq!(i["action"]["buttons"].as_array().unwrap().len(), 3);
        assert_eq!(r.deferred_buttons, 1);
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(10_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s, &KEY).expect("renders");
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
        let r = render(&s, &KEY).expect("renders");
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
        let r = render(&s, &KEY).expect("renders");
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
            dashboard: None,
            interactive: None,
        };
        let body = build_messages_body("491234567", &msg);
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["recipient_type"], "individual");
        assert_eq!(body["to"], "491234567");
        assert_eq!(body["type"], "text");
        assert_eq!(body["text"]["body"], "hello");
        assert_eq!(body["text"]["preview_url"], false);
    }

    #[test]
    fn build_image_message_body_shape_matches_cloud_api() {
        let body = build_image_message_body("491234567", "media_id_stub", Some("a caption"));
        assert_eq!(body["messaging_product"], "whatsapp");
        assert_eq!(body["recipient_type"], "individual");
        assert_eq!(body["to"], "491234567");
        assert_eq!(body["type"], "image");
        assert_eq!(body["image"]["id"], "media_id_stub");
        assert_eq!(body["image"]["caption"], "a caption");
    }

    #[test]
    fn build_image_message_body_omits_caption_when_empty() {
        let body = build_image_message_body("491234567", "m1", None);
        assert!(body["image"].as_object().unwrap().get("caption").is_none());
    }
}
