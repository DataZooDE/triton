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
use triton_core::a2ui::{Component, DashboardTile, Surface, extract_surface};

/// Discord `data.content` hard limit. Documented as 2000 chars
/// (UTF-16 code units); we treat as bytes (conservative).
pub const DISCORD_CONTENT_MAX_BYTES: usize = 2000;

const TRUNCATION_SENTINEL: &str = "\n\n*(truncated — exceeded Discord's 2000-byte content cap)*";

const BUTTON_ONLY_PLACEHOLDER: &str = "Choose an option:";

/// Discord's `components` cap: max 5 Action Rows per message,
/// max 5 Buttons per row → 25 buttons total. Architecture.md
/// §8.7's risk table calls out this exact constraint
/// ("`SurfaceLimits`: Discord 25-item select cap, Telegram
/// 8-buttons-per-row, ..."). The mapper enforces it at L6'
/// before the courier sees an invalid response (Codex PR 22
/// concern).
pub const DISCORD_BUTTONS_PER_ROW: usize = 5;
pub const DISCORD_MAX_ROWS: usize = 5;
pub const DISCORD_MAX_BUTTONS: usize = DISCORD_BUTTONS_PER_ROW * DISCORD_MAX_ROWS;

/// Discord's string-select cap: max 25 options per menu, and
/// each select menu occupies a whole Action Row (one menu per
/// row, no other components alongside). PR 25 enforces both at
/// the mapper edge (Codex PR 22 selection-coverage concern).
pub const DISCORD_SELECT_MAX_OPTIONS: usize = 25;

#[derive(Debug, Clone)]
pub struct RenderedInteraction {
    pub content: String,
    pub components: Option<Value>,
    /// Count of `Component::Button` entries we encountered but
    /// did not render (oversized correlation tokens, button-cap
    /// overflow beyond DISCORD_MAX_BUTTONS, or row-cap
    /// exhaustion competing with Selection menus).
    pub deferred_buttons: usize,
    /// Count of `Component::Selection` entries we encountered but
    /// did not render (oversized correlation tokens, empty
    /// options, options past the 25-cap, or row-cap exhaustion).
    /// PR 25 ships native string-select menus otherwise.
    pub deferred_selections: usize,
    /// Count of `Component::Form` entries we encountered. PR 25
    /// doesn't ship Discord Modal forms yet; the next PR opens
    /// modals via interaction-response type 9 + interaction-type
    /// 5 MODAL_SUBMIT (Codex PR 25 nit — previously folded into
    /// `deferred_buttons`).
    pub deferred_forms: usize,
    /// Count of `Component::Dashboard` entries we couldn't ship.
    /// PR 38 wires the rasterizer, so the FIRST Dashboard in a
    /// surface is surfaced via [`Self::dashboard`] for the adapter
    /// to rasterise + attach to the interaction response. Any
    /// subsequent dashboards bump this counter (one image per
    /// channel-message attachment slot is enough — multi-dashboard
    /// surfaces are an L6′ degrade-rule excess we drop rather
    /// than chunk).
    pub deferred_dashboards: usize,
    pub truncated: bool,
    /// PR 38: the FIRST `Component::Dashboard` found in the surface,
    /// surfaced to the caller for rasterisation. The adapter calls
    /// the rasterizer service and builds a `multipart/form-data`
    /// interaction response carrying the rendered PNG instead of
    /// a plain JSON channel-message. Mirrors Telegram PR 36's
    /// `RenderedMessage::dashboard` slot.
    pub dashboard: Option<RasterDashboard>,
}

/// Dashboard payload the caller passes to the rasterizer. Same
/// shape as Telegram's `surface_mapper::RasterDashboard` — keeping
/// the type local rather than sharing across crates because the
/// Discord/Telegram/WhatsApp mappers each own their L6′ rendering
/// concerns and a shared type would couple them unnecessarily.
#[derive(Debug, Clone)]
pub struct RasterDashboard {
    pub title: String,
    pub tiles: Vec<DashboardTile>,
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
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    let mut deferred_dashboards = 0usize;
    let mut buttons: Vec<Value> = Vec::new();
    let mut select_rows: Vec<Value> = Vec::new();
    // PR 38: first Dashboard surfaced to the caller for rasterisation.
    // Subsequent dashboards bump `deferred_dashboards` (one PNG per
    // channel-message attachment slot is enough — multi-dashboard
    // surfaces are an L6′ degrade-rule excess).
    let mut first_dashboard: Option<RasterDashboard> = None;
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(md_escape(value));
            }
            // `Report` is an optional inline chart rendered out-of-band by
            // adapters that support it (Google Chat); ignored by the text mapper.
            Component::Report { .. } => {}
            // Click-to-open document references degrade to a plain label
            // list on a surface with no embeddable resource host.
            Component::Sources { items } => {
                if !items.is_empty() {
                    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
                    chunks.push(format!("Sources: {}", labels.join(" \u{b7} ")));
                }
            }
            Component::Narration { text } => {
                chunks.push(format!("*{}*", md_escape(text)));
            }
            Component::Button {
                label, tool, args, ..
            } => {
                if buttons.len() >= DISCORD_MAX_BUTTONS {
                    // Beyond Discord's 5x5 grid — `SurfaceLimits`
                    // says reject at the mapper edge before the
                    // courier sees the envelope (architecture.md
                    // §8.7 risk table). Count + tracing::warn is
                    // the v0.2 shape; pagination is future work.
                    deferred_buttons += 1;
                    continue;
                }
                match triton_correlation::encode(tool, args, correlation_key) {
                    Ok(token) => {
                        buttons.push(json!({
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
            // PR 25: native Discord string-select menu. The
            // callback comes back as a component interaction with
            // `data.values: ["<chosen_value>"]`; the adapter's
            // inbound handler reads the args_key sentinel
            // (encoded as the only key in `args` with a `null`
            // value) and substitutes the picked value before
            // dispatching. Each select menu owns a whole Action
            // Row (Discord rule) so they count against the same
            // 5-row total budget as button rows.
            Component::Selection {
                prompt,
                options,
                tool,
                args_key,
            } => {
                // Codex PR 25 concern: defer empty option lists.
                // Discord rejects `options: []` on string-select
                // menus, so an empty Selection has to surface as
                // a deferred render, not a malformed component.
                if options.is_empty() || options.len() > DISCORD_SELECT_MAX_OPTIONS {
                    deferred_selections += 1;
                    // Defer means we don't ship the prompt either —
                    // sending the prompt with no control attached
                    // would be misleading (Codex concern). The
                    // operator sees the deferral via tracing.
                    continue;
                }
                // Args carry a sentinel: the args_key set to null,
                // signalling the inbound handler should fill it
                // with `data.values[0]`. No extra wire-format change
                // to the correlation token (PR 21 stays
                // compatible). The inbound side strictly validates
                // the `{ <one_key>: null }` shape before
                // substituting; see `handle_message_component`.
                let args = json!({ args_key.as_str(): Value::Null });
                let token = match triton_correlation::encode(tool, &args, correlation_key) {
                    Ok(t) => t,
                    Err(_) => {
                        deferred_selections += 1;
                        continue;
                    }
                };
                let opts: Vec<Value> = options
                    .iter()
                    .map(|o| {
                        json!({
                            "label": o.label,
                            "value": o.value,
                        })
                    })
                    .collect();
                let menu = json!({
                    "type": 3, // STRING_SELECT
                    "custom_id": token,
                    "placeholder": prompt,
                    "options": opts,
                    "min_values": 1,
                    "max_values": 1,
                });
                select_rows.push(json!({
                    "type": 1, // ACTION_ROW (one menu per row)
                    "components": [menu],
                }));
                // The prompt also ships as content for accessibility
                // (screen readers / non-Discord clients that render
                // the message but not the component). We only add
                // it AFTER successfully queueing the row — Codex
                // PR 25 concern: a deferred select with a visible
                // prompt-but-no-control is worse than dropping
                // both.
                chunks.push(md_escape(prompt));
            }
            Component::Form { title, fields, .. } => {
                // Form → Modal (interaction response type 9) is
                // a separate handler shape; PR 25 ships Selection
                // only. Render the form as a text summary so the
                // user sees something, and bump `deferred_forms`
                // (its own counter post-Codex-PR-25-nit).
                let names: Vec<String> = fields.iter().map(|f| md_escape(&f.label)).collect();
                chunks.push(format!("**{}**\n{}", md_escape(title), names.join(", ")));
                deferred_forms += 1;
            }
            Component::Dashboard { title, tiles } => {
                // PR 38: the manifest's `dashboard: rasterised_png`
                // degrade rule now has a real renderer behind it.
                // The mapper surfaces the FIRST Dashboard to the
                // caller via `RenderedInteraction.dashboard`; the
                // caller calls the out-of-process rasterizer and
                // builds a multipart interaction response carrying
                // the rendered PNG.
                //
                // We deliberately do NOT push a chunk for the
                // dashboard: surrounding Text/Narration components
                // become the message content (rendered alongside
                // the attached image), but the dashboard itself is
                // the IMAGE — emitting any placeholder string here
                // would either duplicate content (image + text) or,
                // on rasterizer failure (the lib.rs fallback path),
                // make it look like a successful render. The
                // fallback path constructs its own placeholder when
                // it needs one.
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

    if chunks.is_empty()
        && buttons.is_empty()
        && select_rows.is_empty()
        && first_dashboard.is_none()
    {
        return Err(RenderError::EmptyAfterRender);
    }
    // PR 38: a dashboard-only surface is legitimate — Discord's
    // multipart channel-message response can carry an image with no
    // text content. We only need to synthesise placeholder content
    // for the historical button-only-surface case.
    if chunks.is_empty() && !buttons.is_empty() && first_dashboard.is_none() {
        chunks.push(BUTTON_ONLY_PLACEHOLDER.into());
    }

    let joined = chunks.join("\n\n");
    let (content, truncated) = if joined.len() <= DISCORD_CONTENT_MAX_BYTES {
        (joined, false)
    } else {
        truncate_chunks(&chunks)
    };
    // PR 38: build_send_photo_fields-style flag set up by the caller.

    // Combined row budget: at most 5 Action Rows. Select menus
    // each own a whole row; buttons share rows of 5. Selects come
    // first, then buttons fill the remaining rows.
    let mut all_rows: Vec<Value> = Vec::new();
    let mut rows_left = DISCORD_MAX_ROWS;
    for row in select_rows.into_iter() {
        if rows_left == 0 {
            deferred_selections += 1;
            continue;
        }
        all_rows.push(row);
        rows_left -= 1;
    }
    if !buttons.is_empty() {
        let button_rows: Vec<Vec<Value>> = buttons
            .chunks(DISCORD_BUTTONS_PER_ROW)
            .map(|c| c.to_vec())
            .collect();
        for chunk in button_rows {
            if rows_left == 0 {
                deferred_buttons += chunk.len();
                continue;
            }
            all_rows.push(json!({
                "type": 1, // ACTION_ROW
                "components": chunk,
            }));
            rows_left -= 1;
        }
    }
    let components = if all_rows.is_empty() {
        None
    } else {
        Some(Value::Array(all_rows))
    };

    Ok(RenderedInteraction {
        content,
        components,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        deferred_dashboards,
        truncated,
        dashboard: first_dashboard,
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

/// Markdown-escape and cap a raw plain-text reply (non-A2UI tool
/// result) to the same Discord content limit the mapper enforces
/// elsewhere. Codex PR 22 blocker 1: without this, `bare_text`
/// bypassed both Markdown escape and the 2000-byte cap, so a
/// tool returning `@everyone` or a 10 KB blob would inject /
/// be rejected by Discord.
pub fn clamp_plain_text(raw: &str) -> String {
    let escaped = md_escape(raw);
    if escaped.len() <= DISCORD_CONTENT_MAX_BYTES {
        return escaped;
    }
    let budget = DISCORD_CONTENT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut end = budget.min(escaped.len());
    while end > 0 && !escaped.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = escaped[..end].to_string();
    out.push_str(TRUNCATION_SENTINEL);
    out
}

/// PR 30: render a form-only surface as a Discord modal (type=9
/// interaction response). Returns `Some(modal_response)` when the
/// surface contains exactly one `Component::Form` and nothing else;
/// returns `None` for all other surfaces so the caller falls back
/// to the normal text/components path (where Form continues to
/// render as `**title** + field labels` + `deferred_forms += 1`,
/// matching pre-PR-30 behaviour for mixed surfaces).
///
/// Discord modal constraints (documented):
///   * `custom_id` ≤ 100 chars — our HMAC correlation tokens come
///     in well under that.
///   * `title` ≤ 45 chars.
///   * `components` is a list of Action Rows; each row contains
///     exactly one TEXT_INPUT (component type 4). Max 5 rows ⇒
///     max 5 form fields per modal.
///   * Each TEXT_INPUT.custom_id ≤ 100 chars, label ≤ 45 chars.
///
/// Field-kind handling: Discord modals only support TEXT_INPUT.
/// PR 30 ships STRING fields only; INTEGER/BOOLEAN fields cause
/// us to refuse the modal path (returns None → falls back to
/// deferred text rendering). A later PR can add server-side
/// coercion.
pub fn try_render_form_modal(
    result: &Value,
    correlation_key: &[u8],
) -> Option<Result<Value, FormModalError>> {
    let surface = extract_surface(result).ok()?;
    if surface.components.len() != 1 {
        return None;
    }
    let Component::Form {
        title,
        fields,
        submit_label: _,
        tool,
    } = &surface.components[0]
    else {
        return None;
    };
    Some(build_modal_response(title, fields, tool, correlation_key))
}

/// Maximum number of TEXT_INPUTs we'll put in a single modal. The
/// Discord limit is 5 Action Rows per modal, and one TEXT_INPUT
/// takes one row.
pub const DISCORD_MODAL_MAX_FIELDS: usize = 5;

/// Discord modal title cap.
pub const DISCORD_MODAL_TITLE_MAX: usize = 45;

#[derive(Debug, Clone, PartialEq)]
pub enum FormModalError {
    /// Form has more fields than Discord allows in one modal.
    TooManyFields(usize),
    /// Form has zero fields (Discord rejects empty modals).
    NoFields,
    /// Form field uses an INTEGER or BOOLEAN kind. PR 30 ships
    /// STRING-only; richer coercion is a separate PR.
    UnsupportedFieldKind(String),
    /// Codex PR 30 review: a field with an empty `name`. Discord
    /// requires non-empty `custom_id` on TEXT_INPUT, and the
    /// modal-submit handler treats empty `custom_id` as malformed.
    EmptyFieldName,
    /// Codex PR 30 review: two form fields share the same name.
    /// The correlation token's JSON skeleton can't represent
    /// duplicate keys — it would silently collapse to one,
    /// weakening the "exact field set" invariant the submit
    /// handler relies on. Caller must rename.
    DuplicateFieldName(String),
    /// Correlation token didn't fit Discord's 100-char custom_id
    /// budget (this shouldn't happen for a 5-field form with
    /// reasonable names, but belt-and-braces).
    TokenOversize,
}

fn build_modal_response(
    title: &str,
    fields: &[triton_core::a2ui::FormField],
    submit_tool: &str,
    correlation_key: &[u8],
) -> Result<Value, FormModalError> {
    if fields.is_empty() {
        return Err(FormModalError::NoFields);
    }
    if fields.len() > DISCORD_MODAL_MAX_FIELDS {
        return Err(FormModalError::TooManyFields(fields.len()));
    }
    // PR 30 scope: STRING only. Also (Codex PR 30 review) refuse
    // empty or duplicate field names BEFORE building the JSON
    // skeleton — a duplicate would silently collapse to one key,
    // weakening the "token commits to the exact field set"
    // invariant the modal-submit handler relies on.
    let mut seen = std::collections::HashSet::with_capacity(fields.len());
    for f in fields {
        if !matches!(f.kind, triton_core::a2ui::FormFieldKind::String) {
            return Err(FormModalError::UnsupportedFieldKind(f.name.clone()));
        }
        if f.name.is_empty() {
            return Err(FormModalError::EmptyFieldName);
        }
        if !seen.insert(f.name.as_str()) {
            return Err(FormModalError::DuplicateFieldName(f.name.clone()));
        }
    }

    // Build the args skeleton: { field_name: null, ... }. On
    // submit we substitute the user's input for each null slot,
    // strictly matching what the token committed to. Same
    // null-sentinel pattern as PR 25's Selection callback.
    let mut skeleton = serde_json::Map::with_capacity(fields.len());
    for f in fields {
        skeleton.insert(f.name.clone(), Value::Null);
    }
    let args = Value::Object(skeleton);
    let custom_id = triton_correlation::encode_with_cap(
        submit_tool,
        &args,
        correlation_key,
        triton_correlation::DISCORD_MAX_CUSTOM_ID,
    )
    .map_err(|_| FormModalError::TokenOversize)?;

    let mut components = Vec::with_capacity(fields.len());
    for f in fields {
        // TEXT_INPUT (component type 4) inside an Action Row.
        // `style: 1` = short single-line input.
        let input = json!({
            "type": 4,
            "custom_id": f.name,
            "label": clamp_label(&f.label),
            "style": 1,
            "required": f.required,
        });
        components.push(json!({
            "type": 1, // ACTION_ROW
            "components": [input],
        }));
    }

    Ok(json!({
        "type": 9, // MODAL
        "data": {
            "custom_id": custom_id,
            "title": clamp_title(title),
            "components": components,
        }
    }))
}

fn clamp_title(s: &str) -> String {
    let mut end = s.len().min(DISCORD_MODAL_TITLE_MAX);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn clamp_label(s: &str) -> String {
    let mut end = s.len().min(DISCORD_MODAL_TITLE_MAX);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Build a type-4 channel-message body from a `RenderedInteraction`.
/// Kept around for the adapter's plain-JSON (no-dashboard) path and
/// for parity with the v0.2 rendered-shape unit tests; the rasterizer
/// path (PR 38) bypasses this helper in favour of a multipart body.
#[allow(dead_code)]
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
                    resource: None,
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
                resource: None,
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
    fn many_buttons_chunk_into_five_per_row() {
        // 12 buttons → 3 rows (5, 5, 2). All ship.
        let components = (0..12)
            .map(|i| Component::Button {
                label: format!("b{i}"),
                tool: "narrate".into(),
                args: json!({ "s": format!("p{i}") }),
                resource: None,
            })
            .collect();
        let s = Surface { components };
        let r = render(&s, TEST_KEY).expect("renders");
        let rows = r
            .components
            .expect("components")
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 3, "5+5+2 → 3 rows");
        assert_eq!(rows[0]["components"].as_array().unwrap().len(), 5);
        assert_eq!(rows[1]["components"].as_array().unwrap().len(), 5);
        assert_eq!(rows[2]["components"].as_array().unwrap().len(), 2);
        assert_eq!(r.deferred_buttons, 0);
    }

    #[test]
    fn buttons_beyond_grid_cap_are_deferred() {
        // 30 buttons → 25 ship (5×5), 5 deferred.
        let components = (0..30)
            .map(|i| Component::Button {
                label: format!("b{i}"),
                tool: "narrate".into(),
                args: json!({ "s": format!("p{i}") }),
                resource: None,
            })
            .collect();
        let s = Surface { components };
        let r = render(&s, TEST_KEY).expect("renders");
        let rows = r
            .components
            .expect("components")
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), DISCORD_MAX_ROWS, "max 5 rows");
        let total: usize = rows
            .iter()
            .map(|r| r["components"].as_array().unwrap().len())
            .sum();
        assert_eq!(total, DISCORD_MAX_BUTTONS);
        assert_eq!(r.deferred_buttons, 5);
    }

    #[test]
    fn dashboard_is_surfaced_for_rasterisation_not_rendered_as_text() {
        // PR 38: dashboards now travel out through
        // `RenderedInteraction.dashboard` so the caller can call the
        // rasterizer and build a multipart interaction response. The
        // raw tile content (label, value, trend) must NEVER appear
        // in the content — that would either duplicate the rendered
        // image on success, or silently violate the manifest's
        // `dashboard: rasterised_png` degrade rule on the fallback
        // path (the adapter constructs its own placeholder string).
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
                        trend: Some("+5%".into()),
                    }],
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        // None of the tile content appears in the content — only
        // the leading Text/Narration components do.
        assert!(!r.content.contains("1234"));
        assert!(!r.content.contains("+5%"));
        assert!(!r.content.contains("invocations"));
        assert!(!r.content.contains("Secrets"));
        // First (and only) dashboard surfaced to caller.
        let dash = r.dashboard.as_ref().expect("dashboard surfaced");
        assert_eq!(dash.title, "Secrets");
        assert_eq!(dash.tiles.len(), 1);
        assert_eq!(dash.tiles[0].label, "invocations");
        // First (and only) dashboard does NOT count as deferred —
        // it's the image attachment, not a dropped component.
        assert_eq!(r.deferred_dashboards, 0);
        // Leading text component still ships as content.
        assert!(
            r.content.contains("header"),
            "expected content to carry the leading Text chunk, got: {}",
            r.content,
        );
    }

    #[test]
    fn second_dashboard_is_deferred_first_is_surfaced() {
        // Discord channel messages carry one image attachment. With
        // two Dashboards in the same surface, the first becomes the
        // image and any others bump `deferred_dashboards`.
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
        let r = render(&s, TEST_KEY).expect("renders");
        let dash = r.dashboard.expect("first surfaced");
        assert_eq!(dash.title, "first");
        assert_eq!(r.deferred_dashboards, 1);
    }

    #[test]
    fn dashboard_only_surface_renders_with_empty_content() {
        // A surface that's nothing but a Dashboard is now valid:
        // Discord multipart interaction responses don't require
        // any text content alongside attachments. The mapper
        // returns Ok with an empty `content`; the adapter ships
        // the PNG file part with no caption rather than failing
        // EmptyAfterRender.
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
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.content.is_empty());
        assert!(r.dashboard.is_some());
    }

    #[test]
    fn clamp_plain_text_escapes_and_caps() {
        // bare_text path: a non-Surface tool result containing
        // Markdown injection must be escaped, AND output capped to
        // the Discord content limit. Codex PR 22 blocker 1.
        let injected = "@everyone *please* read this".to_string();
        let s = clamp_plain_text(&injected);
        assert!(s.contains(r"\*please\*"), "italic markers escaped");
        // `@` itself isn't a Markdown metachar in Discord — mention
        // suppression is a separate API flag — but escaping the
        // surrounding asterisks is the relevant defence here.

        let huge = "a".repeat(5000);
        let capped = clamp_plain_text(&huge);
        assert!(capped.len() <= DISCORD_CONTENT_MAX_BYTES);
        assert!(capped.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn selection_renders_as_string_select_menu() {
        use triton_core::a2ui::SelectionOption;
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "header".into(),
                },
                Component::Selection {
                    prompt: "Pick a tone".into(),
                    options: vec![
                        SelectionOption {
                            label: "Friendly".into(),
                            value: "friendly".into(),
                        },
                        SelectionOption {
                            label: "Terse".into(),
                            value: "terse".into(),
                        },
                    ],
                    tool: "narrate".into(),
                    args_key: "subject".into(),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        let rows = r
            .components
            .expect("components")
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 1);
        let menu = &rows[0]["components"][0];
        assert_eq!(menu["type"], 3); // STRING_SELECT
        assert_eq!(menu["placeholder"], "Pick a tone");
        let opts = menu["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["label"], "Friendly");
        assert_eq!(opts[0]["value"], "friendly");
        // Token decodes to (narrate, {subject: null}); the inbound
        // handler fills the null with `data.values[0]` at click.
        let token = menu["custom_id"].as_str().expect("custom_id is string");
        let (tool, args) = triton_correlation::decode(token, TEST_KEY).expect("verifies");
        assert_eq!(tool, "narrate");
        assert!(
            args["subject"].is_null(),
            "args_key slot must be null sentinel"
        );
        assert_eq!(r.deferred_selections, 0);
    }

    #[test]
    fn selection_with_too_many_options_is_deferred() {
        use triton_core::a2ui::SelectionOption;
        let options = (0..DISCORD_SELECT_MAX_OPTIONS + 5)
            .map(|i| SelectionOption {
                label: format!("opt-{i}"),
                value: format!("v-{i}"),
            })
            .collect();
        let s = Surface {
            components: vec![
                Component::Text { value: "x".into() },
                Component::Selection {
                    prompt: "p".into(),
                    options,
                    tool: "narrate".into(),
                    args_key: "subject".into(),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.deferred_selections, 1);
        // No select menu shipped — the text chunk still goes out.
        assert!(
            r.components.is_none()
                || r.components
                    .as_ref()
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .is_empty()
        );
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
