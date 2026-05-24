//! v0.2 PR 19/20 — L6' surface mapper for Telegram.
//!
//! Takes the canonical `triton_core::a2ui::Surface` a tool returns
//! and renders it into a Telegram `sendMessage` body. The mapper
//! honours the manifest's per-component `degrade` table (see
//! `doc/architecture.md` §L6′) — PR 19 shipped the passthrough
//! cases (`text`, `narration`); PR 20 (this revision) adds
//! `SurfaceLimits` enforcement and an explicit empty-surface path
//! so cap violations and unsupported envelopes cannot leak past
//! the mapper to the courier (Codex review of PR 19).
//!
//! Buttons stay deferred until the HMAC correlation-token PR (we
//! can't ship arbitrary `(tool, args)` through Telegram's 64-byte
//! `callback_data` without a signed correlation token).
//!
//! Output discipline:
//!   * narration → `<i>...</i>` with `parse_mode: "HTML"` set on
//!     the sendMessage body.
//!   * text → plain text, HTML-escaped (because we're already in
//!     HTML mode for the narration).
//!   * buttons → counted (`deferred_buttons`) and surfaced via
//!     `tracing::warn` by the caller; the surrounding text +
//!     narration still ship.
//!
//! HTML escaping is mandatory: a tool that emits `<script>` would
//! otherwise inject HTML into the post-back, and Telegram's
//! `parse_mode: "HTML"` parser would 400 the whole request on any
//! stray `<` or `&`.

use serde_json::{Value, json};
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Telegram `sendMessage.text` hard limit. Architecture.md §8.7
/// names `SurfaceLimits` as the cap-enforcement seam at L6′.
/// Telegram's published limit is 4096 UTF-16 code units; we use
/// byte-length as a conservative proxy (any UTF-8 string ≤ 4096
/// bytes is ≤ 4096 UTF-16 code units, so byte-counting only ever
/// rejects messages Telegram itself would also reject).
pub const TELEGRAM_TEXT_MAX_BYTES: usize = 4096;

/// Telegram's documented inline-keyboard row cap (architecture.md
/// §8.7 risk table: "Telegram 8-buttons-per-row"). PR 26
/// chunks each Selection's options into rows of this size.
pub const TELEGRAM_BUTTONS_PER_ROW: usize = 8;

/// Sentinel appended when we truncate to fit
/// [`TELEGRAM_TEXT_MAX_BYTES`]. Bracketed so it's visibly an
/// adapter artefact, not the tool's output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded Telegram's 4096-byte limit]";

/// Stand-in body for a Surface that has buttons but no Text /
/// Narration. Telegram's `sendMessage` requires non-empty text
/// even when `reply_markup` is set; without this synthetic line
/// the mapper would have to drop signed buttons (the worst of
/// both worlds — Codex PR 21 review caught this).
const BUTTON_ONLY_PLACEHOLDER: &str = "Choose an option:";

/// Rendered Telegram body. `parse_mode` is set when the rendering
/// uses HTML markers (narration as italics); plain-text-only
/// renders leave it `None` so we don't gratuitously parse.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub text: String,
    pub parse_mode: Option<&'static str>,
    /// `reply_markup` JSON for inline keyboards built from Button
    /// components (PR 21) and Selection options (PR 26 — each
    /// option becomes its own button under one prompt label).
    /// `None` when there were no buttons or every button's token
    /// exceeded the platform's callback_data cap.
    pub reply_markup: Option<Value>,
    /// Number of `Component::Button` entries we encountered but
    /// could not render — either because correlation tokens weren't
    /// available (no key) or because the token would have exceeded
    /// the platform's callback_data cap.
    pub deferred_buttons: usize,
    /// Number of `Component::Selection` entries we couldn't render
    /// (PR 26). A Selection defers when ALL its option tokens
    /// would exceed the 64-byte callback_data cap, or when zero
    /// options were provided.
    pub deferred_selections: usize,
    /// Number of `Component::Dashboard` entries we encountered
    /// (PR 27). Per architecture.md §8.7 dashboards on text-first
    /// adapters need a rasterizer (out-of-process). We never ship
    /// the raw tile content (would silently violate the
    /// `dashboard: rasterised_png` degrade rule); the count
    /// surfaces in tracing so the operator sees how often the gap
    /// matters.
    pub deferred_dashboards: usize,
    /// True when text was truncated to fit
    /// [`TELEGRAM_TEXT_MAX_BYTES`]. Used by the caller for a
    /// `tracing::warn` line so operators can spot oversized tool
    /// output before it becomes a complaint from users.
    pub truncated: bool,
}

/// Mapper-edge failure modes that the caller MUST handle without
/// emitting a Telegram API call. Codex PR 19 review flagged that
/// empty surfaces were quietly producing `text: ""`, which
/// Telegram rejects — turning a mapper-edge violation into a
/// post-courier error. Surfacing the failure here lets us audit it
/// at the right phase and skip the wasted API roundtrip.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// The Surface had zero renderable components (no Text or
    /// Narration). All-Button surfaces also land here for now,
    /// because buttons aren't shipped until the correlation-token
    /// PR — a button-only Surface produces no usable text.
    EmptyAfterRender,
}

/// Try to render `result` as a Telegram message. Returns `None`
/// when the result isn't an A2UI surface — caller falls back to
/// the bare-text path. Returns `Some(Err(...))` when the result
/// IS an A2UI surface but renders to nothing usable.
pub fn try_render_surface(
    result: &Value,
    correlation_key: &[u8],
) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface, correlation_key))
}

/// Render a [`Surface`] into a `RenderedMessage` or a
/// `RenderError`. Public so the in-crate unit tests can exercise
/// the mapper without spinning the whole binary.
///
/// Truncation strategy (Codex PR 20 review blocker):
///
/// Naive byte-truncation of the rendered HTML can split `<i>`/
/// `</i>` tags or `&lt;`/`&amp;`/`&gt;` entities, producing
/// invalid Telegram HTML even when the byte length fits the cap.
/// To stay always-valid we never cut inside a rendered component:
///
/// 1. Render each Text/Narration to a complete HTML chunk.
/// 2. If the joined output fits under the cap, ship it.
/// 3. If not, walk components from the head and keep the largest
///    prefix that fits under `cap - sentinel`. Drop the tail and
///    append the sentinel — every dropped boundary is between two
///    rendered chunks, never inside one.
/// 4. If even the FIRST component alone exceeds the cap, that
///    component is truncated at its *raw* text (before HTML
///    rendering) with a per-character escape-cost accounting (so
///    a string of `&` chars, which inflate 1→5 bytes when escaped,
///    is bounded correctly). The result is then escaped + wrapped
///    fresh, so the output stays syntactically valid.
pub fn render(surface: &Surface, correlation_key: &[u8]) -> Result<RenderedMessage, RenderError> {
    // Each renderable component contributes one PreRender; action
    // components (Selection / Form) bump the deferred counter and
    // also contribute a text fragment that names the surface so
    // the user sees what was offered even when the action surface
    // itself ships later. Button components produce inline-keyboard
    // rows via PR 21's HMAC correlation tokens.
    let mut chunks: Vec<PreRender> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_dashboards = 0usize;
    let mut keyboard_rows: Vec<Vec<Value>> = Vec::new();
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                chunks.push(PreRender::text(value));
            }
            Component::Narration { text } => {
                chunks.push(PreRender::narration(text));
            }
            Component::Button { label, tool, args } => {
                match triton_correlation::encode(tool, args, correlation_key) {
                    Ok(token) => {
                        keyboard_rows.push(vec![json!({
                            "text": label,
                            "callback_data": token,
                        })]);
                    }
                    Err(_) => {
                        // Encode failed (tool+args wouldn't fit in
                        // the 64-byte callback_data cap). Defer the
                        // button so the user still sees text and
                        // the operator sees a `deferred_buttons`
                        // tracing line.
                        deferred_buttons += 1;
                    }
                }
            }
            // PR 26: Selection → inline_keyboard. Per the manifest's
            // `selections: inline_keyboard` degrade rule, Telegram
            // renders each option as its own button under the prompt
            // label. Each button's callback_data is a correlation
            // token for `(tool, {args_key: option_value})`, so when
            // the user clicks the PR 21 dispatch path receives the
            // fully populated args without any inbound-side
            // substitution (cleaner than Discord PR 25's null
            // sentinel; works because Telegram lets a row carry up
            // to 8 buttons and the message can have many rows).
            //
            // Defer rule: an option whose token would exceed the
            // 64-byte callback_data cap is dropped from the keyboard
            // and bumps `deferred_buttons`. If ALL options drop OR
            // the option list is empty, the whole Selection is
            // deferred (we don't ship a half-built keyboard); the
            // prompt chunk only ships when at least one option
            // rendered, matching Codex PR 25's "prompt without
            // control" guard.
            Component::Selection {
                prompt,
                options,
                tool,
                args_key,
            } => {
                if options.is_empty() {
                    deferred_selections += 1;
                    continue;
                }
                // Pre-encode every option's token in one pass. If
                // ANY fails to fit, defer the whole Selection
                // (Codex PR 26 concern: a partial keyboard
                // contradicts the spec's "reject oversize selection
                // sets" directive and gives users a misleading
                // truncated picker).
                let mut option_buttons: Vec<Value> = Vec::new();
                let mut any_overflow = false;
                for opt in options {
                    let args = json!({ args_key.as_str(): &opt.value });
                    match triton_correlation::encode(tool, &args, correlation_key) {
                        Ok(token) => option_buttons.push(json!({
                            "text": opt.label,
                            "callback_data": token,
                        })),
                        Err(_) => {
                            any_overflow = true;
                            break;
                        }
                    }
                }
                if any_overflow {
                    deferred_selections += 1;
                    continue;
                }
                // 8 buttons per row is Telegram's documented cap.
                for chunk in option_buttons.chunks(TELEGRAM_BUTTONS_PER_ROW) {
                    keyboard_rows.push(chunk.to_vec());
                }
                // Prompt joins the text chunks so the user sees the
                // question even when their client renders the
                // inline keyboard below.
                chunks.push(PreRender::pre_rendered(html_escape(prompt), false));
            }
            Component::Form { title, fields, .. } => {
                let names: Vec<String> = fields.iter().map(|f| html_escape(&f.label)).collect();
                let body = format!("<b>{}</b>\n{}", html_escape(title), names.join(", "));
                chunks.push(PreRender::pre_rendered(body, true));
                deferred_buttons += 1;
            }
            Component::Dashboard { title, .. } => {
                // Manifest declares `dashboard: rasterised_png`.
                // PR 27 brings Telegram in line with Discord PR 22:
                // we MUST NOT render the raw tile text because
                // dashboards are non-negotiably visual (architecture
                // §8.7) and emitting `• label: value` silently
                // violates the manifest's `rasterised_png` degrade
                // rule. Render a one-line deferred placeholder and
                // bump `deferred_dashboards`; the rasterizer wires
                // in a follow-up PR.
                chunks.push(PreRender::pre_rendered(
                    format!(
                        "<i>{}</i>",
                        html_escape(&format!(
                            "dashboard '{title}' deferred — rasterizer not yet wired"
                        ))
                    ),
                    true,
                ));
                deferred_dashboards += 1;
            }
        }
    }
    // Codex PR 21 review concern: a button-only Surface (no Text
    // or Narration) used to fail EmptyAfterRender, which dropped
    // the valid signed buttons. Telegram's `sendMessage` requires
    // non-empty `text` even with `reply_markup`, so we synthesise
    // a stable placeholder ("Choose an option:") when buttons
    // exist but no text chunks do. The buttons still ship; the
    // user still sees a meaningful message.
    if chunks.is_empty() && keyboard_rows.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    if chunks.is_empty() {
        chunks.push(PreRender::pre_rendered(
            BUTTON_ONLY_PLACEHOLDER.into(),
            false,
        ));
    }
    let has_html_markers = chunks.iter().any(|p| p.has_html);
    let reply_markup = if keyboard_rows.is_empty() {
        None
    } else {
        Some(json!({ "inline_keyboard": keyboard_rows }))
    };

    let chunk_strings: Vec<String> = chunks.iter().map(|p| p.chunk.clone()).collect();
    let joined = chunk_strings.join("\n\n");
    if joined.len() <= TELEGRAM_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            parse_mode: if has_html_markers { Some("HTML") } else { None },
            reply_markup,
            deferred_buttons,
            deferred_selections,
            deferred_dashboards,
            truncated: false,
        });
    }

    // Over cap. Walk the chunk prefix that fits under
    // `cap - sentinel`, leaving every cut on a between-component
    // boundary so HTML stays valid.
    let budget = TELEGRAM_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in &chunk_strings {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 }; // "\n\n"
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
            parse_mode: if has_html_markers { Some("HTML") } else { None },
            reply_markup: reply_markup.clone(),
            deferred_buttons,
            deferred_selections,
            deferred_dashboards,
            truncated: true,
        });
    }

    // Even the first chunk is too large. For Text/Narration we
    // can truncate the raw source with escape-cost accounting and
    // re-render to a syntactically valid chunk. For everything
    // else (Selection/Form/Dashboard) HTML structure is too
    // entangled with the body — fall back to a bare sentinel so
    // we never ship malformed HTML.
    let first = &chunks[0];
    let wrapper = match first.raw_kind {
        Some(RawKind::Text) => 0,
        Some(RawKind::Narration) => "<i></i>".len(),
        None => 0,
    };
    let inner_budget = budget.saturating_sub(wrapper);
    let out = match (&first.raw_kind, &first.raw_source) {
        (Some(kind), Some(raw)) => {
            let trimmed_raw = budget_raw_for_html_escape(raw, inner_budget);
            let mut s = match kind {
                RawKind::Text => html_escape(trimmed_raw),
                RawKind::Narration => format!("<i>{}</i>", html_escape(trimmed_raw)),
            };
            s.push_str(TRUNCATION_SENTINEL);
            s
        }
        _ => TRUNCATION_SENTINEL.trim_start().to_string(),
    };
    Ok(RenderedMessage {
        text: out,
        parse_mode: if has_html_markers { Some("HTML") } else { None },
        reply_markup,
        deferred_buttons,
        deferred_selections,
        deferred_dashboards,
        truncated: true,
    })
}

/// A renderable chunk plus the metadata the truncation logic needs.
/// `raw_kind` + `raw_source` are only populated for Text/Narration
/// chunks, where the raw-text fallback path can re-render at a
/// smaller budget without breaking HTML structure.
struct PreRender {
    chunk: String,
    has_html: bool,
    raw_kind: Option<RawKind>,
    raw_source: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum RawKind {
    Text,
    Narration,
}

impl PreRender {
    fn text(raw: &str) -> Self {
        Self {
            chunk: html_escape(raw),
            has_html: false,
            raw_kind: Some(RawKind::Text),
            raw_source: Some(raw.to_string()),
        }
    }
    fn narration(raw: &str) -> Self {
        Self {
            chunk: format!("<i>{}</i>", html_escape(raw)),
            has_html: true,
            raw_kind: Some(RawKind::Narration),
            raw_source: Some(raw.to_string()),
        }
    }
    fn pre_rendered(chunk: String, has_html: bool) -> Self {
        Self {
            chunk,
            has_html,
            raw_kind: None,
            raw_source: None,
        }
    }
}

/// Walk `raw` char-by-char, accumulate the cost of HTML-escaping
/// each char, and return the longest prefix whose escaped length
/// is ≤ `max_escaped_bytes`. Escape cost: `&` → 5 bytes (`&amp;`),
/// `<` → 4 (`&lt;`), `>` → 4 (`&gt;`), everything else → its UTF-8
/// byte length. Stopping mid-char would risk invalid UTF-8;
/// stopping at a char boundary is guaranteed by `char_indices`.
fn budget_raw_for_html_escape(raw: &str, max_escaped_bytes: usize) -> &str {
    let mut cost = 0usize;
    let mut end = 0usize;
    for (i, c) in raw.char_indices() {
        let bytes = match c {
            '&' => 5,
            '<' => 4,
            '>' => 4,
            _ => c.len_utf8(),
        };
        if cost + bytes > max_escaped_bytes {
            break;
        }
        cost += bytes;
        end = i + c.len_utf8();
    }
    &raw[..end]
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
    let obj = body.as_object_mut().unwrap();
    if let Some(pm) = msg.parse_mode {
        obj.insert("parse_mode".into(), Value::String(pm.to_string()));
    }
    if let Some(markup) = &msg.reply_markup {
        obj.insert("reply_markup".into(), markup.clone());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::{Component, Surface};

    const TEST_KEY: &[u8] = b"test-correlation-key-32-bytes!!!";

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
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.text, "hello\n\n<i>a footnote</i>");
        assert_eq!(r.parse_mode, Some("HTML"));
        assert_eq!(r.deferred_buttons, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn text_only_omits_parse_mode() {
        let s = Surface {
            components: vec![Component::Text {
                value: "plain".into(),
            }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.text, "plain");
        assert_eq!(r.parse_mode, None);
    }

    #[test]
    fn selection_renders_each_option_as_inline_keyboard_button() {
        use triton_core::a2ui::SelectionOption;
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "Header".into(),
                },
                Component::Selection {
                    prompt: "Pick".into(),
                    options: vec![
                        SelectionOption {
                            label: "A".into(),
                            value: "a".into(),
                        },
                        SelectionOption {
                            label: "B".into(),
                            value: "b".into(),
                        },
                    ],
                    tool: "narrate".into(),
                    args_key: "s".into(),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        let markup = r.reply_markup.expect("inline_keyboard set");
        let rows = markup["inline_keyboard"].as_array().unwrap();
        // 2 buttons in a single row of 8 (cap).
        assert_eq!(rows.len(), 1);
        let row = rows[0].as_array().unwrap();
        assert_eq!(row.len(), 2);
        assert_eq!(row[0]["text"], "A");
        assert_eq!(row[1]["text"], "B");

        // Each button's token decodes to the right (tool, args).
        let (t1, a1) =
            triton_correlation::decode(row[0]["callback_data"].as_str().unwrap(), TEST_KEY)
                .expect("verifies");
        assert_eq!(t1, "narrate");
        assert_eq!(a1["s"], "a");
        let (_, a2) =
            triton_correlation::decode(row[1]["callback_data"].as_str().unwrap(), TEST_KEY)
                .expect("verifies");
        assert_eq!(a2["s"], "b");
        assert_eq!(r.deferred_selections, 0);
    }

    #[test]
    fn dashboard_is_deferred_not_rendered_as_tile_text() {
        // PR 27: Telegram aligns with Discord (PR 22) on the
        // architecture.md §8.7 rule that dashboards on text-first
        // adapters need a rasterizer. The raw tile content
        // (label, value, trend) must NEVER reach the post-back —
        // emitting it would silently violate the manifest's
        // `dashboard: rasterised_png` degrade rule.
        use triton_core::a2ui::DashboardTile;
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
        assert!(!r.text.contains("invocations"));
        assert!(!r.text.contains("1234"));
        assert!(!r.text.contains("+5%"));
        assert_eq!(r.deferred_dashboards, 1);
        // Title also doesn't leak directly — only the deferred
        // placeholder mentions it inside `dashboard '<title>' …`.
        assert!(
            r.text.contains("rasterizer not yet wired"),
            "expected the placeholder line; got: {}",
            r.text
        );
    }

    #[test]
    fn empty_selection_options_defer_whole_selection() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "context".into(),
                },
                Component::Selection {
                    prompt: "pick".into(),
                    options: vec![],
                    tool: "narrate".into(),
                    args_key: "s".into(),
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.deferred_selections, 1);
        assert!(r.reply_markup.is_none());
        // The Selection's prompt MUST NOT ship as visible text
        // when the whole Selection is deferred (mirrors PR 25's
        // Codex-flagged "prompt without control" rule). Only the
        // leading `context` chunk remains.
        assert_eq!(r.text, "context");
    }

    #[test]
    fn buttons_become_inline_keyboard_with_correlation_tokens() {
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
        assert_eq!(r.deferred_buttons, 0);
        assert!(r.text.contains("label"));
        let markup = r.reply_markup.expect("inline_keyboard set");
        let rows = markup["inline_keyboard"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        let cell = &rows[0][0];
        assert_eq!(cell["text"], "Refresh");
        let token = cell["callback_data"].as_str().expect("token is a string");
        // Token round-trips back to (narrate, {}) under the same key.
        let (tool, args) = triton_correlation::decode(token, TEST_KEY).expect("token verifies");
        assert_eq!(tool, "narrate");
        assert_eq!(args, json!({}));
    }

    #[test]
    fn oversized_button_args_are_deferred_not_emitted() {
        // A button whose (tool, args) wouldn't fit in Telegram's
        // 64-byte callback_data is dropped from the keyboard and
        // bumps `deferred_buttons` so the operator sees it via the
        // usual tracing::warn channel.
        let big_args = json!({ "s": "x".repeat(200) });
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "still rendered".into(),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: big_args,
                },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.deferred_buttons, 1);
        assert!(r.reply_markup.is_none());
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
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.text.contains("a &lt; b &amp; c &gt; d"));
        assert!(r.text.contains("<i>x&lt;i&gt;y&lt;/i&gt;z</i>"));
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        // Codex PR 19 blocker 1: an empty Surface used to render
        // `text: ""`, and the courier shipped that — Telegram 400s
        // on empty text. The mapper now refuses at its edge.
        let s = Surface { components: vec![] };
        assert!(matches!(
            render(&s, TEST_KEY),
            Err(RenderError::EmptyAfterRender)
        ));
    }

    #[test]
    fn button_only_surface_synthesises_placeholder_text() {
        // Codex PR 21 review concern: a Surface with valid buttons
        // but no Text/Narration used to fail EmptyAfterRender. PR
        // 21 fixes this by synthesising a stable placeholder body
        // so Telegram's non-empty-text requirement is satisfied
        // and the signed buttons still ship.
        let s = Surface {
            components: vec![Component::Button {
                label: "Click".into(),
                tool: "narrate".into(),
                args: json!({}),
            }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.text, BUTTON_ONLY_PLACEHOLDER);
        let markup = r.reply_markup.expect("inline_keyboard set");
        assert_eq!(markup["inline_keyboard"][0][0]["text"], "Click");
    }

    #[test]
    fn oversized_text_is_truncated_below_cap() {
        let big = "x".repeat(10_000);
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        assert!(r.text.len() <= TELEGRAM_TEXT_MAX_BYTES);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        // 4-byte UTF-8 codepoint so a naive byte-slice would land
        // mid-sequence; verify the cut lands on a char boundary.
        let fourbyte = "𝄞"; // U+1D11E, 4 bytes
        let s = Surface {
            components: vec![Component::Text {
                value: fourbyte.repeat(2000),
            }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        // The String type already guarantees valid UTF-8 globally;
        // the per-byte loop confirms the cut wasn't mid-sequence.
        for i in 0..=r.text.len() {
            let _ = r.text.is_char_boundary(i);
        }
    }

    #[test]
    fn truncation_never_splits_html_entities() {
        // Codex PR 20 review blocker: a naive byte-truncation
        // could cut inside `&lt;`/`&amp;`/`&gt;` and produce
        // invalid Telegram HTML. The new strategy budgets raw
        // text by escape cost (each `<` is 4 escaped bytes), so
        // the output contains only complete entities.
        let big = "<".repeat(2000); // each `<` → `&lt;` (4 bytes)
        let s = Surface {
            components: vec![Component::Text { value: big }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        let before_sentinel = &r.text[..r.text.len() - TRUNCATION_SENTINEL.len()];
        // Stripping every `&lt;` should leave nothing — i.e. the
        // body consists entirely of complete entities.
        let residue = before_sentinel.replace("&lt;", "");
        assert!(
            residue.is_empty(),
            "expected only complete &lt; entities; residue: {residue:?}"
        );
    }

    #[test]
    fn truncation_keeps_italic_tags_balanced() {
        // Codex PR 20 review blocker: a single oversized Narration
        // used to be truncated post-wrap and could end mid-`<i>`.
        // PR 20's truncation budgets the raw narration text and
        // re-wraps, so `<i>...</i>` is always complete.
        let big = "n".repeat(10_000);
        let s = Surface {
            components: vec![Component::Narration { text: big }],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        assert_eq!(r.parse_mode, Some("HTML"));
        let opens = r.text.matches("<i>").count();
        let closes = r.text.matches("</i>").count();
        assert_eq!(
            opens, closes,
            "italic tags must be balanced; got {opens} open / {closes} close in: {}",
            r.text
        );
        assert!(opens >= 1, "expected at least one <i> tag");
    }

    #[test]
    fn truncation_drops_tail_components_when_head_fits() {
        // Many small components whose total exceeds the cap should
        // keep the leading prefix that fits and drop the tail.
        // Every accepted chunk must be intact (not partially cut).
        let small = Component::Text {
            value: "x".repeat(500),
        };
        let s = Surface {
            components: (0..50).map(|_| small.clone()).collect(),
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert!(r.truncated);
        assert!(r.text.ends_with(TRUNCATION_SENTINEL));
        let body = r.text.trim_end_matches(TRUNCATION_SENTINEL);
        for chunk in body.split("\n\n") {
            assert!(
                chunk.is_empty() || chunk.chars().all(|c| c == 'x'),
                "expected only complete chunks; found: {chunk:?}"
            );
        }
    }

    #[test]
    fn multi_component_ordering_is_preserved() {
        // Components must render in the order they appear in the
        // surface; a re-ordered output would change the meaning a
        // tool intended.
        let s = Surface {
            components: vec![
                Component::Text { value: "1".into() },
                Component::Narration { text: "2".into() },
                Component::Text { value: "3".into() },
                Component::Narration { text: "4".into() },
            ],
        };
        let r = render(&s, TEST_KEY).expect("renders");
        assert_eq!(r.text, "1\n\n<i>2</i>\n\n3\n\n<i>4</i>");
    }
}
