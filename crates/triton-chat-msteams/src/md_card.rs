//! Agent Markdown → Adaptive Card body elements.
//!
//! The agent answers in portable Markdown. Pasted whole into one `TextBlock`,
//! Teams renders it with its own Markdown styling: `#`/`##`/`###` headings
//! come out at title size, tables get heavy gridlines, and emoji or
//! `(`query_id`)` tags in a heading read as noise. On a business card that
//! looks unfinished.
//!
//! This module splits the Markdown into blocks and maps each to a native
//! element instead:
//!
//! * heading → one bold `TextBlock` at body or `Medium` size, with leading
//!   emoji and inline-code id tags removed;
//! * table → an AC 1.5 `Table`: tinted header row, no gridlines, numeric
//!   columns right-aligned;
//! * `---` → a `separator` on the next element;
//! * paragraphs and lists → plain `TextBlock`s (inline Markdown still renders).
//!
//! Text without a heading or a table is left as a single `TextBlock`, exactly
//! as before, so short replies and every other caller are unchanged.

use serde_json::{Value, json};

/// The rendered body plus whether it needs AC 1.5 (a `Table` is present).
pub struct CardBody {
    pub elements: Vec<Value>,
    pub has_table: bool,
}

/// True when the Markdown has a heading or a table: the shapes this module
/// restyles. Plain prose stays on the unchanged single-`TextBlock` path.
pub fn is_rich_markdown(md: &str) -> bool {
    let lines: Vec<&str> = md.lines().collect();
    lines.iter().enumerate().any(|(i, l)| {
        heading_level(l).is_some()
            || (is_table_row(l) && lines.get(i + 1).is_some_and(|n| is_table_separator(n)))
    })
}

/// Map agent Markdown to Adaptive Card body elements.
pub fn markdown_to_body(md: &str) -> CardBody {
    if !is_rich_markdown(md) {
        let elements = if md.trim().is_empty() {
            Vec::new()
        } else {
            vec![json!({ "type": "TextBlock", "text": md, "wrap": true })]
        };
        return CardBody {
            elements,
            has_table: false,
        };
    }

    let lines: Vec<&str> = md.lines().collect();
    let mut out = Builder::default();
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        if t.is_empty() {
            out.paragraph(&mut para);
        } else if let Some(level) = heading_level(line) {
            out.paragraph(&mut para);
            out.heading(level, &t[level..]);
        } else if is_rule(t) {
            out.paragraph(&mut para);
            out.separator_next = true;
        } else if is_table_row(line) && lines.get(i + 1).is_some_and(|n| is_table_separator(n)) {
            out.paragraph(&mut para);
            let header = split_row(line);
            let aligns = split_row(lines[i + 1]);
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() && is_table_row(lines[i]) {
                rows.push(split_row(lines[i]));
                i += 1;
            }
            out.table(header, &aligns, rows);
            continue;
        } else {
            // A list starting right under a sentence would run into it on one
            // line in Teams; give the list its own block.
            if !para.is_empty() && is_list_item(line) != is_list_item(para[para.len() - 1]) {
                out.paragraph(&mut para);
            }
            para.push(line);
        }
        i += 1;
    }
    out.paragraph(&mut para);
    CardBody {
        elements: out.elements,
        has_table: out.has_table,
    }
}

#[derive(Default)]
struct Builder {
    elements: Vec<Value>,
    separator_next: bool,
    has_table: bool,
}

impl Builder {
    fn push(&mut self, mut el: Value, spacing: &str) {
        if let Some(prev) = self.elements.last() {
            // Text right under a table needs air, or it reads as a last row.
            let after_table = prev["type"] == "Table";
            el["spacing"] = json!(if after_table { "Medium" } else { spacing });
        }
        if std::mem::take(&mut self.separator_next) && !self.elements.is_empty() {
            el["separator"] = json!(true);
            el["spacing"] = json!("Medium");
        }
        self.elements.push(el);
    }

    fn paragraph(&mut self, para: &mut Vec<&str>) {
        if para.is_empty() {
            return;
        }
        let text = para.join("\n");
        para.clear();
        self.push(
            json!({ "type": "TextBlock", "text": text, "wrap": true }),
            "Small",
        );
    }

    fn heading(&mut self, level: usize, raw: &str) {
        let text = clean_heading(raw);
        if text.is_empty() {
            return;
        }
        let size = if level <= 2 { "Medium" } else { "Default" };
        self.push(
            json!({
                "type": "TextBlock", "text": text, "wrap": true,
                "weight": "Bolder", "size": size,
            }),
            "Medium",
        );
    }

    fn table(&mut self, header: Vec<String>, aligns: &[String], rows: Vec<Vec<String>>) {
        let ncols = header
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        let cell_at = |row: &[String], c: usize| row.get(c).cloned().unwrap_or_default();
        let columns: Vec<Value> = (0..ncols)
            .map(|c| {
                let numeric =
                    !rows.is_empty() && rows.iter().all(|r| is_numeric_cell(&cell_at(r, c)));
                let align = if numeric {
                    "Right"
                } else {
                    match aligns.get(c).map(String::as_str) {
                        Some(a) if a.starts_with(':') && a.ends_with(':') => "Center",
                        Some(a) if a.ends_with(':') => "Right",
                        _ => "Left",
                    }
                };
                // Relative width from the longest cell, so a name column
                // gets room and a percentage column stays narrow.
                let longest = std::iter::once(cell_at(&header, c))
                    .chain(rows.iter().map(|r| cell_at(r, c)))
                    .map(|s| s.chars().count())
                    .max()
                    .unwrap_or(1);
                // Text columns get at least twice a number column's share.
                let min = if numeric { 1 } else { 2 };
                json!({
                    "width": (longest / 8).clamp(min, 4),
                    "horizontalCellContentAlignment": align,
                })
            })
            .collect();
        let row_json = |cells: &[String], header: bool| -> Value {
            let cells: Vec<Value> = (0..ncols)
                .map(|c| {
                    let mut tb = json!({
                        "type": "TextBlock", "text": cell_at(cells, c), "wrap": true,
                    });
                    if header {
                        tb["weight"] = json!("Bolder");
                    }
                    json!({ "type": "TableCell", "items": [tb] })
                })
                .collect();
            let mut row = json!({ "type": "TableRow", "cells": cells });
            if header {
                row["style"] = json!("emphasis");
            }
            row
        };
        let mut all = vec![row_json(&header, true)];
        all.extend(rows.iter().map(|r| row_json(r, false)));
        self.has_table = true;
        self.push(
            json!({
                "type": "Table",
                "columns": columns,
                "rows": all,
                "firstRowAsHeaders": true,
                "showGridLines": false,
            }),
            "Medium",
        );
    }
}

/// `#`..`######` followed by a space: the number of `#`s.
fn heading_level(line: &str) -> Option<usize> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&level) && t[level..].starts_with(' ')).then_some(level)
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

fn is_table_separator(line: &str) -> bool {
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

fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && t[digits..].starts_with(". ")
}

fn is_numeric_cell(cell: &str) -> bool {
    let t = cell.trim().trim_matches('*').trim();
    if t.is_empty() || matches!(t, "—" | "-" | "–") {
        return true;
    }
    t.chars().any(|c| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_digit() || " $€£%.,+-−()/×xd".contains(c) || c.is_whitespace())
}

/// A heading reads as a label: no `#` tail, no leading emoji or bullets, no
/// inline-code id tags such as `(\`top_customers\`)`, no bold markers.
fn clean_heading(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('#').trim().to_string();
    // Drop "(`id`)" tags, then any remaining backticks.
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

    const REPORT: &str = "### 💼 Top Customers & Revenue\n\
        The report (`top-categories`) is attached. Total is **$9,001.50**.\n\
        \n\
        ---\n\
        \n\
        ## 1. Top Customers by Revenue (`top_customers`)\n\
        **Initech** leads.\n\
        | Rank | Customer | Revenue (USD) | Share |\n\
        | :---: | :--- | :---: | :---: |\n\
        | 1 | **Initech** | $2,500.75 | 27.78% |\n\
        | 2 | Stark | $1,750.00 | 19.44% |\n\
        | **Total** | — | **$9,001.50** | **100.0%** |\n\
        \n\
        Key points:\n\
        - Widgets lead\n\
        - Services trail\n";

    fn types(b: &CardBody) -> Vec<&str> {
        b.elements
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn plain_prose_stays_one_textblock() {
        let b = markdown_to_body("Hello **there**.\n\n- one\n- two");
        assert_eq!(types(&b), ["TextBlock"]);
        assert_eq!(b.elements[0]["text"], "Hello **there**.\n\n- one\n- two");
        assert!(!b.has_table);
        assert!(!is_rich_markdown("Hello"));
        assert!(markdown_to_body("  ").elements.is_empty());
    }

    #[test]
    fn headings_become_compact_bold_labels() {
        let b = markdown_to_body(REPORT);
        let h: Vec<&Value> = b
            .elements
            .iter()
            .filter(|e| e["weight"] == "Bolder" && e["type"] == "TextBlock")
            .collect();
        assert_eq!(h[0]["text"], "Top Customers & Revenue");
        assert_eq!(h[0]["size"], "Default"); // ###
        assert_eq!(h[1]["text"], "1. Top Customers by Revenue");
        assert_eq!(h[1]["size"], "Medium"); // ##
        // Never the title sizes Teams' own Markdown renderer uses.
        assert!(
            b.elements
                .iter()
                .all(|e| e["size"] != "Large" && e["size"] != "ExtraLarge")
        );
    }

    #[test]
    fn a_rule_becomes_a_separator_on_the_next_element() {
        let b = markdown_to_body(REPORT);
        let i = b
            .elements
            .iter()
            .position(|e| e["text"] == "1. Top Customers by Revenue")
            .unwrap();
        assert_eq!(b.elements[i]["separator"], true);
        assert_eq!(
            b.elements.iter().filter(|e| e["separator"] == true).count(),
            1
        );
    }

    #[test]
    fn a_markdown_table_becomes_a_native_table() {
        let b = markdown_to_body(REPORT);
        assert!(b.has_table);
        let t = b.elements.iter().find(|e| e["type"] == "Table").unwrap();
        assert_eq!(t["showGridLines"], false);
        assert_eq!(t["firstRowAsHeaders"], true);
        let rows = t["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["style"], "emphasis");
        assert_eq!(rows[0]["cells"][1]["items"][0]["text"], "Customer");
        assert_eq!(rows[0]["cells"][1]["items"][0]["weight"], "Bolder");
        assert_eq!(rows[1]["cells"][2]["items"][0]["text"], "$2,500.75");
        let cols = t["columns"].as_array().unwrap();
        // Numbers right-aligned, the name column left despite `:---`/`:---:`.
        assert_eq!(cols[2]["horizontalCellContentAlignment"], "Right");
        assert_eq!(cols[3]["horizontalCellContentAlignment"], "Right");
        assert_eq!(cols[1]["horizontalCellContentAlignment"], "Left");
        assert_eq!(cols[1]["width"], 2);
        assert_eq!(cols[3]["width"], 1);
    }

    #[test]
    fn a_list_under_a_sentence_gets_its_own_block() {
        let b = markdown_to_body(REPORT);
        let last = b.elements.last().unwrap();
        assert_eq!(last["text"], "- Widgets lead\n- Services trail");
        let prev = &b.elements[b.elements.len() - 2];
        assert_eq!(prev["text"], "Key points:");
        // The block right after the table is spaced away from it.
        assert_eq!(prev["spacing"], "Medium");
    }

    #[test]
    fn clean_heading_strips_noise() {
        assert_eq!(
            clean_heading("📊 **Supplier Reliability** (`supplier-reliability`) ##"),
            "Supplier Reliability"
        );
        assert_eq!(
            clean_heading("2. Revenue by Product Category (`q_by_category`)"),
            "2. Revenue by Product Category"
        );
        assert_eq!(
            clean_heading("🧬 Evolve Run: `demo-run`"),
            "Evolve Run: demo-run"
        );
    }
}
