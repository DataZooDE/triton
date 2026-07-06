//! L6′ surface mapper for the Gemini Enterprise answer surface.
//!
//! Gemini renders rich **markdown**, which sets it apart from the text-first
//! chat channels (Telegram/Signal/WhatsApp — plain text) and the card
//! channels (Google Chat / MS Teams — native widgets). The distinctive
//! choices:
//!   * Text / Narration → markdown passthrough (bold, lists, etc. survive).
//!   * Dashboard → an inline markdown **table** (Metric | Value | Trend),
//!     NOT deferred and NOT rasterised — a rich answer surface renders tables.
//!   * Sources → a numbered **citation** list with markdown links
//!     (`1. [label](resource)`), matching how Gemini surfaces grounding.
//!   * Report → a caption line (the chart itself lives in the app; the answer
//!     surface can't embed the iframe, but it names the chart).
//!   * Button / Selection / Form → deferred + counted. The answer surface
//!     doesn't execute arbitrary tool-call controls, so — like Signal — we
//!     defer them honestly rather than fake an affordance.
//!
//! Cap: 8000 bytes — generous, matching Gemini's long-answer surface;
//! truncation appends a sentinel at a chunk (then UTF-8) boundary.

use serde_json::Value;
use triton_core::a2ui::{Component, Surface, extract_surface};

/// Per-answer byte cap. Gemini tolerates long answers; 8 KB is generous but
/// bounded so a runaway surface can't produce an unbounded preview body.
pub const GEMINI_TEXT_MAX_BYTES: usize = 8000;

const TRUNCATION_SENTINEL: &str = "\n\n[truncated — content exceeded Gemini's 8000-byte cap]";

/// Rendered Gemini answer body. Same envelope shape as the chat mappers
/// (`text` + `deferred_*` + `truncated`) so the preview endpoint maps every
/// adapter uniformly; `deferred_dashboards` stays 0 because we render tables.
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
    /// Nothing renderable (no Text/Narration/Dashboard/Sources/Report).
    EmptyAfterRender,
}

/// Try to render `result` as a Gemini answer. `None` when the result isn't an
/// A2UI surface (caller falls back); `Some(Err(_))` when it is but renders to
/// nothing usable.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedMessage, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

/// Render a [`Surface`] into a [`RenderedMessage`] or [`RenderError`]. Public
/// so the in-crate unit tests can exercise the mapper without a running binary.
pub fn render(surface: &Surface) -> Result<RenderedMessage, RenderError> {
    let mut chunks: Vec<String> = Vec::new();
    let mut deferred_buttons = 0usize;
    let mut deferred_selections = 0usize;
    let mut deferred_forms = 0usize;
    for c in &surface.components {
        match c {
            // Markdown passthrough — Gemini renders the emphasis/lists as-is.
            Component::Text { value } => chunks.push(value.clone()),
            Component::Narration { text } => chunks.push(text.clone()),
            Component::Dashboard { title, tiles } => chunks.push(dashboard_table(title, tiles)),
            Component::Sources { items } => {
                if !items.is_empty() {
                    chunks.push(citations(items));
                }
            }
            Component::Report { report_id, .. } => {
                chunks.push(format!("📊 _{report_id}_ — open the chart in the app"));
            }
            Component::Button { .. } => deferred_buttons += 1,
            Component::Selection { .. } => deferred_selections += 1,
            Component::Form { .. } => deferred_forms += 1,
        }
    }
    if chunks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }
    let joined = chunks.join("\n\n");
    let (text, truncated) = cap(joined, &chunks);
    Ok(RenderedMessage {
        text,
        deferred_buttons,
        deferred_selections,
        deferred_forms,
        // Dashboards render inline as tables — never deferred, never rasterised.
        deferred_dashboards: 0,
        truncated,
    })
}

/// A `Dashboard` as an inline markdown table. The Trend column appears only
/// when at least one tile carries a trend, so a trend-less dashboard stays
/// tidy.
fn dashboard_table(title: &str, tiles: &[triton_core::a2ui::DashboardTile]) -> String {
    let any_trend = tiles.iter().any(|t| t.trend.is_some());
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("**{title}**\n\n"));
    }
    if any_trend {
        out.push_str("| Metric | Value | Trend |\n| --- | --- | --- |\n");
        for t in tiles {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                t.label,
                t.value,
                t.trend.as_deref().unwrap_or("")
            ));
        }
    } else {
        out.push_str("| Metric | Value |\n| --- | --- |\n");
        for t in tiles {
            out.push_str(&format!("| {} | {} |\n", t.label, t.value));
        }
    }
    out.trim_end().to_string()
}

/// A `Sources` row as a numbered citation list with markdown links.
fn citations(items: &[triton_core::a2ui::SourceItem]) -> String {
    let mut out = String::from("**References**\n");
    for (i, item) in items.iter().enumerate() {
        out.push_str(&format!("{}. [{}]({})\n", i + 1, item.label, item.resource));
    }
    out.trim_end().to_string()
}

/// Cap the joined body at [`GEMINI_TEXT_MAX_BYTES`], dropping whole trailing
/// chunks first and only cutting mid-chunk (at a UTF-8 boundary) when the very
/// first chunk already overflows. Mirrors the text-first channels' discipline.
fn cap(joined: String, chunks: &[String]) -> (String, bool) {
    if joined.len() <= GEMINI_TEXT_MAX_BYTES {
        return (joined, false);
    }
    let budget = GEMINI_TEXT_MAX_BYTES - TRUNCATION_SENTINEL.len();
    let mut accepted: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for chunk in chunks {
        let sep_cost = if accepted.is_empty() { 0 } else { 2 };
        if total + sep_cost + chunk.len() > budget {
            break;
        }
        total += sep_cost + chunk.len();
        accepted.push(chunk);
    }
    let mut body = if accepted.is_empty() {
        let raw = &chunks[0];
        let mut cut = budget.min(raw.len());
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        raw[..cut].to_string()
    } else {
        accepted.join("\n\n")
    };
    body.push_str(TRUNCATION_SENTINEL);
    (body, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use triton_core::a2ui::{Component, DashboardTile, SourceItem, Surface};

    #[test]
    fn text_and_narration_pass_markdown_through() {
        let s = Surface {
            components: vec![
                Component::Text {
                    value: "Revenue is **up**.".into(),
                },
                Component::Narration {
                    text: "net of disruptions".into(),
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.text, "Revenue is **up**.\n\nnet of disruptions");
        assert!(!r.truncated);
    }

    #[test]
    fn dashboard_becomes_a_markdown_table_not_deferred() {
        let s = Surface {
            components: vec![Component::Dashboard {
                title: "Q3".into(),
                tiles: vec![
                    DashboardTile {
                        label: "Revenue".into(),
                        value: "€48,250".into(),
                        trend: Some("up".into()),
                    },
                    DashboardTile {
                        label: "Open orders".into(),
                        value: "3".into(),
                        trend: None,
                    },
                ],
            }],
        };
        let r = render(&s).expect("renders");
        assert!(r.text.contains("| Metric | Value | Trend |"), "{}", r.text);
        assert!(r.text.contains("| Revenue | €48,250 | up |"), "{}", r.text);
        assert_eq!(r.deferred_dashboards, 0);
    }

    #[test]
    fn sources_become_numbered_citations() {
        let s = Surface {
            components: vec![Component::Sources {
                items: vec![SourceItem {
                    label: "account · initech".into(),
                    resource: "ui://peacock/document?id=initech".into(),
                }],
            }],
        };
        let r = render(&s).expect("renders");
        assert!(
            r.text
                .contains("1. [account · initech](ui://peacock/document?id=initech)"),
            "{}",
            r.text
        );
    }

    #[test]
    fn interactive_controls_are_deferred() {
        let s = Surface {
            components: vec![
                Component::Text { value: "hi".into() },
                Component::Button {
                    label: "Go".into(),
                    tool: "narrate".into(),
                    args: json!({}),
                    resource: None,
                },
            ],
        };
        let r = render(&s).expect("renders");
        assert_eq!(r.deferred_buttons, 1);
        assert!(r.text.contains("hi"));
    }

    #[test]
    fn empty_surface_is_a_render_error() {
        let s = Surface { components: vec![] };
        assert!(matches!(render(&s), Err(RenderError::EmptyAfterRender)));
    }
}
