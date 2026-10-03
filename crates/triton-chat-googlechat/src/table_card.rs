//! Agent Markdown with tables → a lead line plus Cards v2 sections.
//!
//! Google Chat has no table element: in message text a Markdown table shows
//! as raw `| … |` rows, and Cards v2 offers no grid of more than two columns.
//! So a reply that carries a table is split:
//!
//! * the **lead**: the prose before the first heading, table or rule (the
//!   one-sentence answer), stays the message text, so notifications and
//!   previews read naturally;
//! * the **rest** becomes card sections, in order. A heading opens a section
//!   and becomes its header (emoji and inline-code id tags stripped). Prose
//!   becomes a `textParagraph`. A table becomes a bold header row, a
//!   divider, then one `columns` row per line: the first cell left, the other
//!   cells joined with " · " and aligned right.
//!
//! Replies without a table are left alone (`rich_answer` returns `None`).

use serde_json::{Value, json};

use crate::surface_mapper::to_card_html;

/// At most this many body rows per table; Chat caps a card's widget count.
const MAX_TABLE_ROWS: usize = 25;

pub struct RichAnswer {
    /// Markdown before the first heading, table or rule; may be empty.
    pub lead_md: String,
    /// Cards v2 sections for everything after the lead.
    pub sections: Vec<Value>,
}

/// Split table-bearing Markdown into a lead and card sections.
pub fn rich_answer(md: &str) -> Option<RichAnswer> {
    let lines: Vec<&str> = md.lines().collect();
    let starts_table =
        |i: usize| is_table_row(lines[i]) && lines.get(i + 1).is_some_and(|n| is_separator(n));
    if !(0..lines.len()).any(starts_table) {
        return None;
    }

    let mut b = Builder::default();
    let mut para: Vec<&str> = Vec::new();
    let mut lead: Vec<&str> = Vec::new();
    let mut in_lead = true;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        let block_start = heading_text(line).is_some() || is_rule(t) || starts_table(i);
        if in_lead {
            if block_start {
                in_lead = false;
            } else {
                lead.push(line);
                i += 1;
                continue;
            }
        }
        if t.is_empty() {
            b.paragraph(&mut para);
        } else if let Some(h) = heading_text(line) {
            b.paragraph(&mut para);
            b.open(Some(clean_heading(h)));
        } else if is_rule(t) {
            b.paragraph(&mut para);
            b.open(None);
        } else if starts_table(i) {
            b.paragraph(&mut para);
            let header = split_row(line);
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() && is_table_row(lines[i]) {
                rows.push(split_row(lines[i]));
                i += 1;
            }
            b.table(&header, &rows);
            continue;
        } else {
            para.push(line);
        }
        i += 1;
    }
    b.paragraph(&mut para);
    b.close();
    Some(RichAnswer {
        lead_md: lead.join("\n").trim().to_string(),
        sections: b.sections,
    })
}

#[derive(Default)]
struct Builder {
    sections: Vec<Value>,
    header: Option<String>,
    widgets: Vec<Value>,
}

impl Builder {
    /// Start a new section (flushing the current one).
    fn open(&mut self, header: Option<String>) {
        self.close();
        self.header = header.filter(|h| !h.is_empty());
    }

    fn close(&mut self) {
        if self.widgets.is_empty() && self.header.is_none() {
            return;
        }
        let mut s = json!({ "widgets": std::mem::take(&mut self.widgets) });
        if let Some(h) = self.header.take() {
            s["header"] = json!(h);
        }
        // A section that is only a header (heading with nothing under it)
        // still renders the title; Chat requires `widgets`, so keep it.
        if s["widgets"].as_array().is_some_and(Vec::is_empty) {
            s["widgets"] = json!([{ "textParagraph": { "text": "" } }]);
        }
        self.sections.push(s);
    }

    fn paragraph(&mut self, para: &mut Vec<&str>) {
        if para.is_empty() {
            return;
        }
        let html = to_card_html(&para.join("\n"));
        para.clear();
        self.widgets
            .push(json!({ "textParagraph": { "text": html } }));
    }

    fn table(&mut self, header: &[String], rows: &[Vec<String>]) {
        let html = |cell: &str| to_card_html(cell);
        let rest = |cells: &[String]| -> String {
            cells
                .iter()
                .skip(1)
                .map(|c| html(c))
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let row = |left: String, right: String| -> Value {
            if right.is_empty() {
                return json!({ "textParagraph": { "text": left } });
            }
            json!({ "columns": { "columnItems": [
                {
                    "horizontalSizeStyle": "FILL_AVAILABLE_SPACE",
                    "horizontalAlignment": "START",
                    "verticalAlignment": "CENTER",
                    "widgets": [ { "textParagraph": { "text": left } } ]
                },
                {
                    "horizontalSizeStyle": "FILL_MINIMUM_SPACE",
                    "horizontalAlignment": "END",
                    "verticalAlignment": "CENTER",
                    "widgets": [ { "textParagraph": { "text": right } } ]
                }
            ] } })
        };
        let bold = |s: String| {
            if s.is_empty() {
                s
            } else {
                format!("<b>{s}</b>")
            }
        };
        let first = |cells: &[String]| cells.first().map(|c| html(c)).unwrap_or_default();
        self.widgets
            .push(row(bold(first(header)), bold(rest(header))));
        self.widgets.push(json!({ "divider": {} }));
        for r in rows.iter().take(MAX_TABLE_ROWS) {
            self.widgets.push(row(first(r), rest(r)));
        }
        if rows.len() > MAX_TABLE_ROWS {
            let more = rows.len() - MAX_TABLE_ROWS;
            self.widgets.push(json!({
                "textParagraph": { "text": format!("<i>… {more} more rows in the report</i>") }
            }));
        }
    }
}

fn heading_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&level) && t[level..].starts_with(' ')).then(|| &t[level..])
}

fn is_rule(t: &str) -> bool {
    let c: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
    c.len() >= 3
        && (c.iter().all(|&x| x == '-')
            || c.iter().all(|&x| x == '*')
            || c.iter().all(|&x| x == '_'))
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() > 1
}

fn is_separator(line: &str) -> bool {
    is_table_row(line)
        && split_row(line).iter().all(|c| {
            let c = c.trim_matches(':');
            !c.is_empty() && c.chars().all(|x| x == '-')
        })
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_start_matches('|');
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// A section header is plain text: no `#` tail, leading emoji, inline-code id
/// tags such as `(\`top_customers\`)`, backticks or bold markers.
fn clean_heading(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('#').trim().to_string();
    while let Some(start) = s.find("(`") {
        match s[start..].find("`)") {
            Some(end) => s.replace_range(start..start + end + 2, ""),
            None => break,
        }
    }
    s = s.replace('`', "").replace("**", "");
    let s = s.trim_start_matches(|c: char| !c.is_alphanumeric());
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "Baltic Components is the main risk at **55.6%** on time.\n\
        \n\
        ### 📊 Supplier Delivery Performance (`supplier_reliability`)\n\
        | Supplier | On-Time Rate | Lead Time (Median / P90) | Risk Level |\n\
        | :--- | :---: | :---: | :---: |\n\
        | Nordwind Logistics | 88.9% | 4.0 / 5.4 days | Moderate |\n\
        | **Baltic Components** | 55.6% | 7.0 / 12.2 days | High |\n\
        \n\
        ### Details\n\
        - Nordwind: 3 excused deliveries\n\
        - Baltic: plant fire counts against it\n";

    fn row_texts(w: &Value) -> (String, String) {
        let items = &w["columns"]["columnItems"];
        (
            items[0]["widgets"][0]["textParagraph"]["text"]
                .as_str()
                .unwrap()
                .to_string(),
            items[1]["widgets"][0]["textParagraph"]["text"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    #[test]
    fn a_reply_without_a_table_is_left_alone() {
        assert!(rich_answer("Just **text**.\n\n### Heading\n- a\n- b").is_none());
    }

    #[test]
    fn the_lead_sentence_stays_message_text() {
        let r = rich_answer(ANSWER).unwrap();
        assert_eq!(
            r.lead_md,
            "Baltic Components is the main risk at **55.6%** on time."
        );
    }

    #[test]
    fn headings_become_clean_section_headers() {
        let r = rich_answer(ANSWER).unwrap();
        assert_eq!(r.sections.len(), 2);
        assert_eq!(r.sections[0]["header"], "Supplier Delivery Performance");
        assert_eq!(r.sections[1]["header"], "Details");
    }

    #[test]
    fn a_table_becomes_aligned_two_column_rows() {
        let r = rich_answer(ANSWER).unwrap();
        let w = r.sections[0]["widgets"].as_array().unwrap();
        // header row, divider, two body rows
        assert_eq!(w.len(), 4);
        assert_eq!(
            row_texts(&w[0]),
            (
                "<b>Supplier</b>".to_string(),
                "<b>On-Time Rate · Lead Time (Median / P90) · Risk Level</b>".to_string()
            )
        );
        assert!(w[1].get("divider").is_some());
        assert_eq!(
            row_texts(&w[3]),
            (
                "<b>Baltic Components</b>".to_string(),
                "55.6% · 7.0 / 12.2 days · High".to_string()
            )
        );
        let right = &w[2]["columns"]["columnItems"][1];
        assert_eq!(right["horizontalAlignment"], "END");
        assert_eq!(right["horizontalSizeStyle"], "FILL_MINIMUM_SPACE");
        // No raw Markdown table syntax survives anywhere.
        assert!(!serde_json::to_string(&r.sections).unwrap().contains("| "));
    }

    #[test]
    fn prose_after_a_heading_renders_as_card_html() {
        let r = rich_answer(ANSWER).unwrap();
        let t = r.sections[1]["widgets"][0]["textParagraph"]["text"]
            .as_str()
            .unwrap();
        assert!(t.contains("• Nordwind: 3 excused deliveries"), "{t}");
    }

    #[test]
    fn a_one_column_table_needs_no_columns_widget() {
        let r = rich_answer("| Name |\n| --- |\n| Acme |").unwrap();
        assert_eq!(r.lead_md, "");
        let w = r.sections[0]["widgets"].as_array().unwrap();
        assert_eq!(w[2]["textParagraph"]["text"], "Acme");
    }

    #[test]
    fn long_tables_are_capped() {
        let mut md = String::from("| A | B |\n| --- | --- |\n");
        for n in 0..30 {
            md.push_str(&format!("| r{n} | {n} |\n"));
        }
        let r = rich_answer(&md).unwrap();
        let w = r.sections[0]["widgets"].as_array().unwrap();
        assert_eq!(w.len(), 2 + MAX_TABLE_ROWS + 1);
        assert!(
            w.last().unwrap()["textParagraph"]["text"]
                .as_str()
                .unwrap()
                .contains("5 more rows")
        );
    }
}
