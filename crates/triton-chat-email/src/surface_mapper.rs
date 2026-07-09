//! L6′ surface mapper for **email**.
//!
//! Unlike the bubble/card mappers, email renders the surface *complete*:
//!
//! - Text → HTML (`**bold**`, `#` headings, `- ` bullets, `[t](u)` links,
//!   `` `code` `` all normalised to real tags).
//! - Narration → a muted italic paragraph.
//! - Button → a styled `<a>` link (grouped runs share one action row); NOT
//!   deferred.
//! - Selection → the prompt plus a list of option links; NOT deferred.
//! - Dashboard → an HTML `<table>` (Metric / Value / Trend); NOT deferred.
//! - Report → an inline caption (`📊 …`); the delivered email embeds the
//!   rasterised chart, the Explorer preview overlays the live chart it holds.
//! - Sources → a labelled link list.
//! - Form → fields rendered read-only and counted in `deferred_forms` — an
//!   inbox can't POST a tool call, the one interaction email can't host.
//!
//! Every reply also carries a **subject**, derived from the lead text — the
//! one output field no chat mapper produces.

use regex::Regex;
use serde_json::Value;
use triton_core::a2ui::{Component, Surface, extract_surface};

/// A conservative ceiling on the rendered HTML body. Real mail transfer
/// limits are megabytes; this only guards a pathological surface. Cuts land
/// between complete component blocks so tags stay balanced.
pub const EMAIL_HTML_MAX_BYTES: usize = 512_000;

const TRUNCATION_SENTINEL_HTML: &str =
    "<p style=\"color:#999\"><em>[truncated — message exceeded the email size limit]</em></p>";
const TRUNCATION_SENTINEL_TEXT: &str = "\n\n[truncated — message exceeded the email size limit]";

/// Fallback subject when the surface carries no lead text to derive one from.
const DEFAULT_SUBJECT: &str = "A message from your assistant";

/// What the email mapper produces: a subject, an HTML body, and a plaintext
/// alternative. `deferred_forms` counts `Form` components (email can't host an
/// interactive submit); every other component renders, so the remaining
/// `deferred_*` counters are always 0 — kept for envelope uniformity with the
/// other mappers.
#[derive(Debug, Clone)]
pub struct RenderedEmail {
    pub subject: String,
    pub html: String,
    pub text: String,
    pub deferred_buttons: usize,
    pub deferred_selections: usize,
    pub deferred_forms: usize,
    pub deferred_dashboards: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    /// The Surface had zero renderable components.
    EmptyAfterRender,
}

/// Try to render `result` as an email. Returns `None` when the result isn't
/// an A2UI surface (caller falls back to bare text); `Some(Err(..))` when it
/// IS a surface but renders to nothing.
pub fn try_render_surface(result: &Value) -> Option<Result<RenderedEmail, RenderError>> {
    let surface = extract_surface(result).ok()?;
    Some(render(&surface))
}

pub fn render(surface: &Surface) -> Result<RenderedEmail, RenderError> {
    let mut html_blocks: Vec<String> = Vec::new();
    let mut text_blocks: Vec<String> = Vec::new();
    let mut deferred_forms = 0usize;
    // Consecutive buttons group into one action row.
    let mut pending: Vec<(String, String)> = Vec::new();

    // Flush any buffered buttons as a single action row.
    fn flush(
        pending: &mut Vec<(String, String)>,
        html_blocks: &mut Vec<String>,
        text_blocks: &mut Vec<String>,
    ) {
        if pending.is_empty() {
            return;
        }
        let links: Vec<String> = pending
            .iter()
            .map(|(label, href)| {
                format!(
                    "<a href=\"{}\" style=\"display:inline-block;padding:8px 14px;margin:4px 6px 4px 0;\
                     background:#eef;border-radius:8px;color:#1a3;text-decoration:none;font-weight:600\">{}</a>",
                    escape_attr(href),
                    escape_html(label)
                )
            })
            .collect();
        html_blocks.push(format!("<p class=\"actions\">{}</p>", links.join("")));
        let labels: Vec<String> = pending.iter().map(|(l, _)| format!("[{l}]")).collect();
        text_blocks.push(labels.join("  "));
        pending.clear();
    }

    for c in &surface.components {
        match c {
            Component::Text { value } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                html_blocks.push(md_to_html(value));
                // The plaintext alternative drops emphasis fences so it reads
                // cleanly (no stray `**`), keeping the rest of the text as-is.
                text_blocks.push(strip_markdown(value));
            }
            Component::Narration { text } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                html_blocks.push(format!(
                    "<p style=\"color:#666\"><em>{}</em></p>",
                    escape_html(text)
                ));
                text_blocks.push(format!("({text})"));
            }
            Component::Button {
                label, resource, ..
            } => {
                // A `ui://` / `http(s)` resource opens directly; a tool-invoke
                // button with no resource links to `#` here — the outbound
                // courier rewrites it to a deep link when it delivers.
                let href = resource
                    .as_deref()
                    .filter(|r| r.starts_with("ui://") || r.starts_with("http"))
                    .unwrap_or("#")
                    .to_string();
                pending.push((label.clone(), href));
            }
            Component::Selection {
                prompt, options, ..
            } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                let items: Vec<String> = options
                    .iter()
                    .map(|o| format!("<li>{}</li>", escape_html(&o.label)))
                    .collect();
                html_blocks.push(format!(
                    "<p>{}</p><ul>{}</ul>",
                    escape_html(prompt),
                    items.join("")
                ));
                let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
                text_blocks.push(format!("{prompt}\n- {}", labels.join("\n- ")));
            }
            Component::Form { title, fields, .. } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                deferred_forms += 1;
                let rows: Vec<String> = fields
                    .iter()
                    .map(|f| {
                        format!(
                            "<li>{}: <span style=\"color:#999\">____</span></li>",
                            escape_html(&f.label)
                        )
                    })
                    .collect();
                html_blocks.push(format!(
                    "<p><strong>{}</strong></p><ul>{}</ul>\
                     <p style=\"color:#999\"><em>Open in the app to submit.</em></p>",
                    escape_html(title),
                    rows.join("")
                ));
                text_blocks.push(format!("{title} (open in the app to submit)"));
            }
            Component::Dashboard { title, tiles } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                html_blocks.push(dashboard_table(title, tiles));
                let mut t = if title.is_empty() {
                    String::new()
                } else {
                    format!("{title}\n")
                };
                for tile in tiles {
                    t.push_str(&format!("- {}: {}\n", tile.label, tile.value));
                }
                text_blocks.push(t.trim_end().to_string());
            }
            Component::Report { report_id, .. } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                html_blocks.push(format!(
                    "<p>\u{1F4CA} <em>{}</em></p>",
                    escape_html(report_id)
                ));
                text_blocks.push(format!("\u{1F4CA} {report_id}"));
            }
            Component::Sources { items } => {
                flush(&mut pending, &mut html_blocks, &mut text_blocks);
                if !items.is_empty() {
                    let links: Vec<String> = items
                        .iter()
                        .map(|i| {
                            format!(
                                "<li><a href=\"{}\">{}</a></li>",
                                escape_attr(&i.resource),
                                escape_html(&i.label)
                            )
                        })
                        .collect();
                    html_blocks.push(format!(
                        "<p><strong>Sources</strong></p><ul>{}</ul>",
                        links.join("")
                    ));
                    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
                    text_blocks.push(format!("Sources: {}", labels.join(" \u{b7} ")));
                }
            }
        }
    }
    flush(&mut pending, &mut html_blocks, &mut text_blocks);

    if html_blocks.is_empty() {
        return Err(RenderError::EmptyAfterRender);
    }

    let subject = derive_subject(surface);
    let (body_blocks, text_body_blocks, truncated) = truncate_blocks(html_blocks, text_blocks);
    let html = wrap_document(&subject, &body_blocks, truncated);
    let mut text = text_body_blocks.join("\n\n");
    if truncated {
        text.push_str(TRUNCATION_SENTINEL_TEXT);
    }

    Ok(RenderedEmail {
        subject,
        html,
        text,
        deferred_buttons: 0,
        deferred_selections: 0,
        deferred_forms,
        deferred_dashboards: 0,
        truncated,
    })
}

/// Keep the head-run of HTML blocks that fits under [`EMAIL_HTML_MAX_BYTES`]
/// (leaving room for the sentinel), dropping trailing blocks whole so tags
/// stay balanced. The plaintext blocks are trimmed to the same count.
fn truncate_blocks(
    html_blocks: Vec<String>,
    text_blocks: Vec<String>,
) -> (Vec<String>, Vec<String>, bool) {
    let total: usize = html_blocks.iter().map(String::len).sum();
    if total <= EMAIL_HTML_MAX_BYTES {
        return (html_blocks, text_blocks, false);
    }
    let budget = EMAIL_HTML_MAX_BYTES.saturating_sub(TRUNCATION_SENTINEL_HTML.len());
    let mut kept = 0usize;
    let mut running = 0usize;
    for b in &html_blocks {
        if running + b.len() > budget {
            break;
        }
        running += b.len();
        kept += 1;
    }
    // Always keep at least the first block so the mail isn't empty.
    let kept = kept.max(1);
    let mut kept_html: Vec<String> = html_blocks.into_iter().take(kept).collect();
    let mut kept_text: Vec<String> = text_blocks.into_iter().take(kept).collect();
    // Pathological single block larger than the whole budget: cut it at a UTF-8
    // boundary so the ceiling is a genuine hard cap, not soft-by-one-block. This
    // may leave that one block's tags unbalanced, but bounding the size matters
    // more than balance for a surface this degenerate (real blocks are tiny).
    if kept == 1 {
        if let Some(first) = kept_html.first_mut()
            && first.len() > budget
        {
            *first = truncate_str_to_boundary(first, budget);
        }
        if let Some(first) = kept_text.first_mut()
            && first.len() > budget
        {
            *first = truncate_str_to_boundary(first, budget);
        }
    }
    (kept_html, kept_text, true)
}

/// Truncate `s` to the largest UTF-8 char boundary `<= max` bytes.
fn truncate_str_to_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Wrap the rendered blocks in a minimal, self-contained HTML email document.
fn wrap_document(subject: &str, blocks: &[String], truncated: bool) -> String {
    let mut body = blocks.join("\n");
    if truncated {
        body.push('\n');
        body.push_str(TRUNCATION_SENTINEL_HTML);
    }
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\"><title>{}</title></head>\
         <body style=\"font-family:-apple-system,Segoe UI,Roboto,sans-serif;color:#222;\
         max-width:640px;margin:0 auto;padding:16px;line-height:1.5\">\n{}\n</body></html>",
        escape_html(subject),
        body
    )
}

/// Derive the email subject from the surface's lead text: the first non-empty
/// line of the first `Text` component, stripped of markdown emphasis/heading
/// markers and clamped to a sane length. Falls back to [`DEFAULT_SUBJECT`].
pub fn derive_subject(surface: &Surface) -> String {
    let lead = surface.components.iter().find_map(|c| match c {
        Component::Text { value } => value
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_owned),
        _ => None,
    });
    let Some(line) = lead else {
        return DEFAULT_SUBJECT.to_string();
    };
    let stripped = strip_markdown(&line);
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return DEFAULT_SUBJECT.to_string();
    }
    truncate_to_char_boundary(stripped, 120)
}

/// Strip the markdown markers we might see in a subject: leading `#` headers
/// and `**`/`__` bold fences. Inline content is preserved.
fn strip_markdown(s: &str) -> String {
    use std::sync::LazyLock;
    static HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}\s+").unwrap());
    static BOLD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\*\*([^*\n]+)\*\*|__([^_\n]+)__").unwrap());
    let no_header = HEADER.replace(s, "");
    BOLD.replace_all(&no_header, "$1$2").into_owned()
}

/// A `Dashboard` as an HTML table (Metric / Value / Trend). The Trend column
/// only appears when at least one tile carries a trend.
fn dashboard_table(title: &str, tiles: &[triton_core::a2ui::DashboardTile]) -> String {
    let has_trend = tiles.iter().any(|t| t.trend.is_some());
    let mut head = String::from("<tr><th align=\"left\">Metric</th><th align=\"left\">Value</th>");
    if has_trend {
        head.push_str("<th align=\"left\">Trend</th>");
    }
    head.push_str("</tr>");
    let mut rows = String::new();
    for t in tiles {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td>",
            escape_html(&t.label),
            escape_html(&t.value)
        ));
        if has_trend {
            rows.push_str(&format!(
                "<td>{}</td>",
                escape_html(t.trend.as_deref().unwrap_or(""))
            ));
        }
        rows.push_str("</tr>");
    }
    let caption = if title.is_empty() {
        String::new()
    } else {
        format!(
            "<caption style=\"text-align:left;font-weight:600;padding-bottom:4px\">{}</caption>",
            escape_html(title)
        )
    };
    format!(
        "<table cellpadding=\"6\" style=\"border-collapse:collapse;border:1px solid #ddd\">{caption}{head}{rows}</table>"
    )
}

/// Escape the five XML/HTML-special characters for text content.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string destined for a double-quoted attribute value.
fn escape_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

/// Sanitise a markdown-link URL for use as an `href`. The input has already
/// been through [`escape_html`] (so `<`, `>`, `&` are neutralised), leaving two
/// gaps this closes: (1) a `"` can still break out of the double-quoted
/// attribute, and (2) any scheme — including `javascript:` / `data:` — would
/// otherwise pass through. Only `http`, `https`, `mailto` and `ui://` links are
/// kept; anything else collapses to `#`. Ampersands are left as their existing
/// `&amp;` (no double-escaping).
fn sanitize_href(escaped_url: &str) -> String {
    let lower = escaped_url.trim().to_ascii_lowercase();
    let allowed = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("ui://");
    if !allowed {
        return "#".to_string();
    }
    // `escape_html` already handled `&`/`<`/`>`; only the attribute-breaking
    // quote remains to neutralise.
    escaped_url.replace('"', "&quot;")
}

/// Normalise the model's portable markdown into an HTML fragment: `#` headers
/// → `<h3>`, `- `/`* ` bullets → `<ul><li>`, blank-line-separated runs → `<p>`
/// (soft line breaks become `<br>`), and inline `**bold**`/`__bold__`,
/// `[text](url)` links and `` `code` `` spans. Text is HTML-escaped first, so
/// tags we emit are the only markup in the output.
fn md_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut para: Vec<String> = Vec::new();
    let mut list: Vec<String> = Vec::new();

    use std::sync::LazyLock;
    static HEADER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}\s+(.*?)\s*$").unwrap());
    static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*]\s+(.*)$").unwrap());

    for raw in md.lines() {
        if raw.trim().is_empty() {
            flush_para(&mut out, &mut para);
            flush_list(&mut out, &mut list);
        } else if let Some(c) = HEADER.captures(raw) {
            flush_para(&mut out, &mut para);
            flush_list(&mut out, &mut list);
            out.push_str(&format!("<h3>{}</h3>", inline_md(&c[1])));
        } else if let Some(c) = BULLET.captures(raw) {
            flush_para(&mut out, &mut para);
            list.push(format!("<li>{}</li>", inline_md(&c[1])));
        } else {
            flush_list(&mut out, &mut list);
            para.push(inline_md(raw));
        }
    }
    flush_para(&mut out, &mut para);
    flush_list(&mut out, &mut list);
    out
}

fn flush_para(out: &mut String, para: &mut Vec<String>) {
    if !para.is_empty() {
        out.push_str(&format!("<p>{}</p>", para.join("<br>")));
        para.clear();
    }
}

fn flush_list(out: &mut String, list: &mut Vec<String>) {
    if !list.is_empty() {
        out.push_str(&format!("<ul>{}</ul>", list.join("")));
        list.clear();
    }
}

/// Inline markdown → HTML on one already-block-classified line. Escapes first,
/// then links (before bold, so a `[` inside doesn't trip the bold pass), then
/// bold, then inline code.
fn inline_md(s: &str) -> String {
    use std::sync::LazyLock;
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").unwrap());
    static BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+)\*\*").unwrap());
    static BOLD_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__([^_\n]+)__").unwrap());
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());

    let escaped = escape_html(s);
    // The link text (`$1`) is already HTML-escaped; the URL (`$2`) is not safe
    // for a double-quoted attribute (`escape_html` leaves `"` untouched) and is
    // not scheme-checked. Route it through `sanitize_href` so a crafted link
    // can neither break out of the `href` nor smuggle a `javascript:`/`data:`
    // scheme into the delivered mail — matching the Button/Sources href paths.
    let linked = LINK.replace_all(&escaped, |c: &regex::Captures| {
        format!("<a href=\"{}\">{}</a>", sanitize_href(&c[2]), &c[1])
    });
    let bold = BOLD_STAR.replace_all(&linked, "<strong>$1</strong>");
    let bold = BOLD_UNDER.replace_all(&bold, "<strong>$1</strong>");
    CODE.replace_all(&bold, "<code>$1</code>").into_owned()
}

/// UTF-8-safe truncation at the largest char boundary `<= max`, appending an
/// ellipsis when it actually cuts.
fn truncate_to_char_boundary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{}…", truncated.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::{
        Component, DashboardTile, FormField, FormFieldKind, SelectionOption, SourceItem, Surface,
    };

    fn surface(components: Vec<Component>) -> Surface {
        Surface { components }
    }

    #[test]
    fn text_becomes_html_with_subject_from_lead() {
        let s = surface(vec![Component::Text {
            value: "**Initech** leads revenue.\n\n- widgets\n- services".into(),
        }]);
        let r = render(&s).expect("renders");
        assert_eq!(r.subject, "Initech leads revenue.");
        assert!(r.html.contains("<strong>Initech</strong>"));
        assert!(
            r.html
                .contains("<ul><li>widgets</li><li>services</li></ul>")
        );
        assert!(r.html.starts_with("<!doctype html>"));
        assert!(r.html.contains("<title>Initech leads revenue.</title>"));
        // Plaintext alternative carries the raw text.
        assert!(r.text.contains("Initech leads revenue."));
        assert!(!r.truncated);
    }

    #[test]
    fn buttons_render_as_links_not_deferred() {
        let s = surface(vec![
            Component::Text {
                value: "Top customers".into(),
            },
            Component::Button {
                label: "What does Initech buy?".into(),
                tool: "assistant".into(),
                args: serde_json::json!({ "question": "what does initech buy?" }),
                resource: None,
            },
            Component::Button {
                label: "Open report".into(),
                tool: "render_report".into(),
                args: serde_json::json!({}),
                resource: Some("ui://peacock/sales?x=1".into()),
            },
        ]);
        let r = render(&s).expect("renders");
        assert_eq!(r.deferred_buttons, 0, "email renders buttons, never defers");
        // Both labels appear as <a> links; the resource-bearing one carries its href.
        assert!(r.html.contains(">What does Initech buy?</a>"));
        assert!(r.html.contains("href=\"ui://peacock/sales?x=1\""));
        // Consecutive buttons share one action row.
        assert_eq!(r.html.matches("class=\"actions\"").count(), 1);
    }

    #[test]
    fn dashboard_becomes_a_table_not_deferred() {
        let s = surface(vec![Component::Dashboard {
            title: "Revenue".into(),
            tiles: vec![
                DashboardTile {
                    label: "Initech".into(),
                    value: "$2,500".into(),
                    trend: Some("▲".into()),
                },
                DashboardTile {
                    label: "Stark".into(),
                    value: "$1,750".into(),
                    trend: None,
                },
            ],
        }]);
        let r = render(&s).expect("renders");
        assert_eq!(r.deferred_dashboards, 0);
        assert!(r.html.contains("<table"));
        assert!(r.html.contains("<th align=\"left\">Trend</th>"));
        assert!(r.html.contains("<td>Initech</td><td>$2,500</td>"));
        assert!(r.html.contains("<caption"));
    }

    #[test]
    fn report_renders_a_caption() {
        let s = surface(vec![
            Component::Text {
                value: "See the chart".into(),
            },
            Component::Report {
                report_id: "sales-by-customer".into(),
                args: serde_json::json!({}),
            },
        ]);
        let r = render(&s).expect("renders");
        assert!(r.html.contains("<em>sales-by-customer</em>"));
        assert!(r.text.contains("sales-by-customer"));
    }

    #[test]
    fn form_fields_render_read_only_and_defer_the_submit() {
        let s = surface(vec![Component::Form {
            title: "Allocate".into(),
            fields: vec![FormField {
                name: "material".into(),
                label: "Material".into(),
                kind: FormFieldKind::String,
                required: true,
            }],
            submit_label: "Go".into(),
            tool: "assistant".into(),
        }]);
        let r = render(&s).expect("renders");
        assert_eq!(
            r.deferred_forms, 1,
            "email can't host an interactive submit"
        );
        assert!(r.html.contains("<strong>Allocate</strong>"));
        assert!(r.html.contains("Material: <span"));
    }

    #[test]
    fn selection_and_sources_render_as_lists() {
        let s = surface(vec![
            Component::Selection {
                prompt: "Pick".into(),
                options: vec![SelectionOption {
                    label: "Alpine".into(),
                    value: "a".into(),
                }],
                tool: "assistant".into(),
                args_key: "q".into(),
            },
            Component::Sources {
                items: vec![SourceItem {
                    label: "account · initech".into(),
                    resource: "ui://peacock/document?id=initech".into(),
                }],
            },
        ]);
        let r = render(&s).expect("renders");
        assert!(r.html.contains("<li>Alpine</li>"));
        assert!(r.html.contains("<strong>Sources</strong>"));
        assert!(r.html.contains("href=\"ui://peacock/document?id=initech\""));
    }

    #[test]
    fn html_special_chars_are_escaped() {
        let s = surface(vec![Component::Text {
            value: "5 < 10 & \"quoted\" > tag".into(),
        }]);
        let r = render(&s).expect("renders");
        assert!(r.html.contains("5 &lt; 10 &amp; \"quoted\" &gt; tag"));
        // No raw angle bracket from the content leaked into the body.
        assert!(!r.html.contains("< 10"));
    }

    #[test]
    fn empty_surface_is_render_error() {
        assert!(matches!(
            render(&surface(vec![])),
            Err(RenderError::EmptyAfterRender)
        ));
    }

    #[test]
    fn no_lead_text_falls_back_to_default_subject() {
        let s = surface(vec![Component::Dashboard {
            title: "Revenue".into(),
            tiles: vec![],
        }]);
        let r = render(&s).expect("renders");
        assert_eq!(r.subject, DEFAULT_SUBJECT);
    }

    #[test]
    fn subject_strips_heading_and_bold_and_clamps() {
        let s = surface(vec![Component::Text {
            value: "## **Q3 renewal** at risk for the whole account portfolio and then some more text that runs well beyond the clamp limit to force truncation here".into(),
        }]);
        let r = render(&s).expect("renders");
        assert!(r.subject.starts_with("Q3 renewal at risk"));
        assert!(r.subject.chars().count() <= 121); // 120 + ellipsis
        assert!(r.subject.ends_with('…'));
    }

    #[test]
    fn oversized_body_truncates_between_blocks() {
        let big = Component::Text {
            value: "x".repeat(4_000),
        };
        let s = surface((0..200).map(|_| big.clone()).collect());
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(r.html.len() <= EMAIL_HTML_MAX_BYTES + 512); // + document chrome
        assert!(r.html.contains("truncated"));
        assert!(r.html.ends_with("</body></html>"));
    }

    #[test]
    fn try_render_surface_none_on_non_surface() {
        assert!(try_render_surface(&serde_json::json!({ "echo": "x" })).is_none());
    }

    #[test]
    fn markdown_link_cannot_break_out_of_the_href_attribute() {
        // A `"` inside a link URL must not escape the double-quoted href and
        // inject event-handler attributes into the delivered HTML. (Clean lead
        // line so the derived subject/title doesn't echo the payload.)
        let s = surface(vec![Component::Text {
            value: "Summary.\n\nsee [click](https://x\"onmouseover=alert(1))".into(),
        }]);
        let r = render(&s).expect("renders");
        // No unescaped quote immediately preceding a handler — the attribute
        // was not broken; the quote is rendered as `&quot;`.
        assert!(
            !r.html.contains("\"onmouseover"),
            "attribute breakout leaked: {}",
            r.html
        );
        assert!(r.html.contains("&quot;onmouseover"));
    }

    #[test]
    fn markdown_link_with_dangerous_scheme_is_neutralised() {
        // `javascript:` / `data:` hrefs must be dropped to a safe placeholder.
        // Lead line is clean so the assertions target the rendered body hrefs.
        let s = surface(vec![Component::Text {
            value: "Options.\n\ntap [here](javascript:alert(1)) or [x](data:text/html,<b>)".into(),
        }]);
        let r = render(&s).expect("renders");
        // Both dangerous links collapse to the safe placeholder href.
        assert_eq!(r.html.matches("href=\"#\"").count(), 2);
        assert!(
            !r.html.contains("href=\"javascript:"),
            "js scheme leaked: {}",
            r.html
        );
        assert!(
            !r.html.contains("href=\"data:"),
            "data scheme leaked: {}",
            r.html
        );
        // The link text still renders; only the href is defanged.
        assert!(r.html.contains(">here</a>"));
    }

    #[test]
    fn markdown_link_with_allowed_scheme_is_preserved() {
        let s = surface(vec![Component::Text {
            value:
                "open [report](https://example.com/r?a=1&b=2) and [doc](ui://peacock/document?id=x)"
                    .into(),
        }]);
        let r = render(&s).expect("renders");
        // http(s) and ui:// survive; the `&` is the correctly-escaped `&amp;`.
        assert!(
            r.html
                .contains("href=\"https://example.com/r?a=1&amp;b=2\"")
        );
        assert!(r.html.contains("href=\"ui://peacock/document?id=x\""));
        // No double-escaping of the ampersand.
        assert!(!r.html.contains("&amp;amp;"));
    }

    #[test]
    fn single_oversized_block_is_hard_capped() {
        // A single block larger than the whole budget must still be bounded —
        // the ceiling is a hard cap, not soft-by-one-block.
        let s = surface(vec![Component::Text {
            value: "y".repeat(EMAIL_HTML_MAX_BYTES + 50_000),
        }]);
        let r = render(&s).expect("renders");
        assert!(r.truncated);
        assert!(
            r.html.len() <= EMAIL_HTML_MAX_BYTES + 512,
            "html exceeded the hard cap: {} bytes",
            r.html.len()
        );
        assert!(r.html.ends_with("</body></html>"));
    }
}
