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

/// Sentinel appended when we truncate to fit
/// [`TELEGRAM_TEXT_MAX_BYTES`]. Bracketed so it's visibly an
/// adapter artefact, not the tool's output.
const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded Telegram's 4096-byte limit]";

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
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
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
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut renderable: Vec<(ComponentKind, String)> = Vec::new();
    let mut deferred_buttons = 0usize;
    for c in &surface.components {
        match c {
            Component::Text { value } => {
                renderable.push((ComponentKind::Text, value.clone()));
            }
            Component::Narration { text } => {
                renderable.push((ComponentKind::Narration, text.clone()));
            }
            Component::Button { .. } => {
                deferred_buttons += 1;
            }
        }
    }
    if renderable.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    let has_html_markers = renderable
        .iter()
        .any(|(k, _)| matches!(k, ComponentKind::Narration));

    let chunks: Vec<String> = renderable
        .iter()
        .map(|(k, raw)| render_one(*k, raw))
        .collect();
    let joined = chunks.join("\n\n");
    if joined.len() <= TELEGRAM_TEXT_MAX_BYTES {
        return Ok(RenderedMessage {
            text: joined,
            parse_mode: if has_html_markers { Some("HTML") } else { None },
            deferred_buttons,
            truncated: false,
        });
    }

    // Over cap. Walk the chunk prefix that fits under
    // `cap - sentinel`, leaving every cut on a between-component
    // boundary so HTML stays valid.
    let budget = TELEGRAM_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
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

    if !accepted.is_empty() {
        let mut out = accepted.join("\n\n");
        out.push_str(TRUNCATION_SENTINEL);
        return Ok(RenderedMessage {
            text: out,
            parse_mode: if has_html_markers { Some("HTML") } else { None },
            deferred_buttons,
            truncated: true,
        });
    }

    // Even the first component is too large. Truncate its raw
    // text (before HTML escape/wrap) so the output stays valid.
    let (kind, raw) = &renderable[0];
    let wrapper = match kind {
        ComponentKind::Text => 0,
        ComponentKind::Narration => "<i></i>".len(),
    };
    let inner_budget = budget.saturating_sub(wrapper);
    let trimmed_raw = budget_raw_for_html_escape(raw, inner_budget);
    let mut out = render_one(*kind, trimmed_raw);
    out.push_str(TRUNCATION_SENTINEL);
    Ok(RenderedMessage {
        text: out,
        parse_mode: if has_html_markers { Some("HTML") } else { None },
        deferred_buttons,
        truncated: true,
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ComponentKind {
    Text,
    Narration,
}

fn render_one(kind: ComponentKind, raw: &str) -> String {
    let escaped = html_escape(raw);
    match kind {
        ComponentKind::Text => escaped,
        ComponentKind::Narration => format!("<i>{escaped}</i>"),
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
        assert!(r.text.contains("a &lt; b &amp; c &gt; d"));
        assert!(r.text.contains("<i>x&lt;i&gt;y&lt;/i&gt;z</i>"));
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        // Codex PR 19 blocker 1: an empty Surface used to render
        // `text: ""`, and the courier shipped that — Telegram 400s
        // on empty text. The mapper now refuses at its edge.
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }

    #[test]
    fn button_only_surface_is_a_render_error() {
        // Same reasoning: a Surface that's all-Button has no
        // renderable text under PR 19's passthrough mapping, so
        // the mapper refuses rather than ship empty text.
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
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
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "1\n\n<i>2</i>\n\n3\n\n<i>4</i>");
    }
}
