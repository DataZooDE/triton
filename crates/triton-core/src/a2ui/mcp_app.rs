//! The canonical surface as an **MCP Apps** widget (ChatGPT, Claude).
//!
//! An MCP host that renders MCP Apps (SEP-1865, extension
//! `io.modelcontextprotocol/ui`, spec 2026-01-26) loads ONE static HTML
//! resource for a tool and hands it each `tools/call` result. So the
//! mapping has two halves:
//!
//! * [`widget_html`]: the static, self-contained widget page served at
//!   [`resource_uri`] with [`MIME_TYPE`]. It bundles the official A2UI v0.9
//!   renderer (`@a2ui/lit`, basic catalog; built from `packages/mcp-widget`)
//!   plus Triton's MCP Apps glue, and loads nothing over the network;
//! * [`card`]: the per-answer A2UI v0.9 messages it renders, carried in the
//!   result `_meta` under [`META_KEY`] (the host hands result `_meta` to the
//!   widget, not to the model): the producer's basic-catalog passthrough
//!   surfaces, then Triton's own card ([`super::basic`]).
//!
//! Prose (`text`, `narration`) stays out of the card: the host shows the
//! tool's text content, and the card must not say it twice. The reverse
//! holds too: on a host that renders the card, the text content does not
//! repeat the card's rows. It is [`card_text`] (title and row names), then
//! the narration, charts and sources the card does not show.

use serde_json::Value;

use super::basic::{self, CardSummary};
use super::passthrough::{self, Catalog};

/// The MCP Apps resource mime type (ext-apps spec, `RESOURCE_MIME_TYPE`).
pub const MIME_TYPE: &str = "text/html;profile=mcp-app";

/// The `tools/call` result `_meta` key that carries the A2UI v0.9 messages.
pub const META_KEY: &str = "triton/a2ui";

const HTML: &str = include_str!("mcp_widget.html");

/// The static widget page.
pub fn widget_html() -> &'static str {
    HTML
}

/// The widget's `ui://` URI. It names a hash of the page, so a host that
/// caches resources by URI (ChatGPT does) picks up a changed widget.
pub fn resource_uri() -> &'static str {
    static URI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URI.get_or_init(|| {
        // FNV-1a, 64 bit: a cache key, not a security property.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in HTML.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("ui://triton/answer-card-{:08x}.html", h >> 32)
    })
}

/// True for any revision of the widget's URI: a host that listed the tools
/// before an upgrade still gets the current page.
pub fn is_widget_uri(uri: &str) -> bool {
    uri.strip_prefix("ui://triton/answer-card")
        .is_some_and(|rest| rest.ends_with(".html") && !rest.contains('/'))
}

/// What the widget renders for one tool result.
#[derive(Debug, Clone)]
pub struct McpCard {
    /// The v0.9 messages, for result `_meta[META_KEY]`.
    pub messages: Vec<Value>,
    /// Triton's own card's summary (`None` with passthrough surfaces only).
    pub summary: Option<CardSummary>,
}

/// The widget's messages for a tool result: the producer's vetted
/// basic-catalog passthrough surfaces (the renderer knows no other
/// catalog), then Triton's card. `None` when there is nothing to draw.
pub fn card(result: &Value) -> Option<McpCard> {
    let mut messages = passthrough::messages(
        result.get("surface").and_then(|s| s.get("a2ui")),
        &[Catalog::Basic],
        passthrough::BASIC_CATALOG_ID,
    );
    let own = basic::project(result);
    let summary = own.as_ref().map(|c| c.summary.clone());
    messages.extend(own.into_iter().flat_map(|c| c.messages));
    (!messages.is_empty()).then_some(McpCard { messages, summary })
}

/// Names the card text lists at most; the card itself shows the rest.
pub const SUMMARY_NAMES: usize = 20;
/// Characters of one name in the card text.
pub const SUMMARY_NAME_CHARS: usize = 60;

/// What a card-rendering host's model is told after the card text, unless
/// the deployment sets its own note (`TRITON_MCP_CARD_NOTE`).
pub const CARD_NOTE: &str =
    "The card shows these rows to the user; add at most one sentence and do not repeat them.";

/// The model-visible text for a host that renders the card, in place of the
/// full text: one line with the card's title and the names of its rows, then
/// `note`. `None` when the card has no rows, or was cut by a budget: then
/// the full text stays, because the card does not show everything the text
/// says.
pub fn card_text(s: &CardSummary, note: &str) -> Option<String> {
    if s.cut {
        return None;
    }
    let mut names: Vec<String> = s
        .names
        .iter()
        .map(|n| row_name(n))
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    let title = s
        .title
        .as_deref()
        .or(s.block_title.as_deref())
        .map(|t| t.trim().trim_end_matches(['.', ':', ';']).trim_end())
        .filter(|t| !t.is_empty());
    let context = s
        .context
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let more = names.len().saturating_sub(SUMMARY_NAMES);
    names.truncate(SUMMARY_NAMES);
    let mut line = String::new();
    if let Some(t) = title {
        line.push_str(t);
        if let Some(c) = context {
            line.push_str(&format!(" ({c})"));
        }
        line.push_str(": ");
    }
    line.push_str(&names.join(", "));
    if more > 0 {
        line.push_str(&format!(", \u{2026} and {more} more"));
    }
    line.push_str(" \u{2014} shown in the card.");
    let note = note.trim();
    if !note.is_empty() {
        line.push('\n');
        line.push_str(note);
    }
    Some(line)
}

/// A row's name in the card text: the first part of a label that packs
/// its details (`name · detail`), at most [`SUMMARY_NAME_CHARS`].
fn row_name(label: &str) -> String {
    let name = label.split(" \u{b7} ").next().unwrap_or(label).trim();
    if name.chars().count() <= SUMMARY_NAME_CHARS {
        return name.to_string();
    }
    let mut cut: String = name.chars().take(SUMMARY_NAME_CHARS - 1).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(result: Value) -> CardSummary {
        card(&result).expect("card").summary.expect("own card")
    }

    #[test]
    fn the_uri_names_the_page_revision() {
        let uri = resource_uri();
        assert!(is_widget_uri(uri), "{uri}");
        assert!(is_widget_uri("ui://triton/answer-card-00000000.html"));
        assert!(!is_widget_uri("ui://triton/runtime.html"));
        assert!(!is_widget_uri("ui://triton/answer-card/../x.html"));
        assert!(!is_widget_uri("ui://peacock/answer-card.html"));
    }

    fn table_card(names: &[&str]) -> CardSummary {
        let rows: Vec<Value> = names
            .iter()
            .map(|n| json!({ "cells": { "name": n, "owner": "Ops" } }))
            .collect();
        summary(json!({ "surface": { "components": [
            { "kind": "header", "title": "6 Things are open.", "stamp": "as of 9:00" },
            { "kind": "table", "columns": [ { "key": "name", "label": "Name" },
                { "key": "owner", "label": "Owner" } ], "rows": rows }
        ] } }))
    }

    /// One line: the card's title and its row names, then the note.
    #[test]
    fn the_card_text_is_the_title_and_the_row_names() {
        let d = table_card(&["Thing 3", "Thing 8", "Thing 13"]);
        assert_eq!(
            card_text(&d, CARD_NOTE).as_deref(),
            Some(
                "6 Things are open: Thing 3, Thing 8, Thing 13 \u{2014} shown in the card.\n\
                 The card shows these rows to the user; add at most one sentence and do not repeat them."
            )
        );
        // A deployment's own note replaces the neutral one; an empty one is none.
        assert_eq!(
            card_text(&d, "Details via the lookup tool.")
                .unwrap()
                .lines()
                .last(),
            Some("Details via the lookup tool.")
        );
        assert_eq!(card_text(&d, "  ").unwrap().lines().count(), 1);
    }

    /// Many rows: the first names and a count of the rest.
    #[test]
    fn a_long_card_text_names_the_first_rows_and_counts_the_rest() {
        let names: Vec<String> = (1..=SUMMARY_NAMES + 5).map(|i| format!("T{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let line = card_text(&table_card(&refs), "").unwrap();
        assert!(
            line.contains(&format!("T{SUMMARY_NAMES}, \u{2026} and 5 more")),
            "{line}"
        );
        assert!(!line.contains(&format!("T{}", SUMMARY_NAMES + 1)), "{line}");
    }

    /// Lists and trees name their rows too; a card without rows has no card
    /// text (the full text stays).
    #[test]
    fn lists_and_trees_have_card_text_and_other_cards_none() {
        let list = summary(json!({ "surface": { "components": [
            { "kind": "list", "title": "Open drafts", "items": [ { "label": "A" }, { "label": "B" } ] }
        ] } }));
        assert_eq!(
            card_text(&list, "").as_deref(),
            Some("Open drafts: A, B \u{2014} shown in the card.")
        );
        let tree = summary(json!({ "surface": { "components": [
            { "kind": "header", "title": "Map" },
            { "kind": "tree", "nodes": [ { "label": "Sales", "children": [ { "label": "Leads" } ] },
                { "label": "Finance" } ] }
        ] } }));
        assert_eq!(
            card_text(&tree, "").as_deref(),
            Some("Map: Sales, Finance \u{2014} shown in the card.")
        );
        let facts = summary(json!({ "surface": { "components": [
            { "kind": "header", "title": "Agent 3" },
            { "kind": "key_value", "pairs": [ { "label": "Owner", "value": "Ops" } ] }
        ] } }));
        assert_eq!(card_text(&facts, CARD_NOTE), None);
    }

    /// A row label that packs its details (`name · detail · detail`) is
    /// named by its first part, and a long name is cut.
    #[test]
    fn a_row_is_named_by_its_first_part() {
        let long = "L".repeat(SUMMARY_NAME_CHARS + 10);
        let list = summary(json!({ "surface": { "components": [
            { "kind": "list", "items": [ { "label": "Thing 3 \u{b7} Ops \u{b7} Head" },
                { "label": long } ] }
        ] } }));
        let line = card_text(&list, "").unwrap();
        let cut: String = "L".repeat(SUMMARY_NAME_CHARS - 1);
        assert_eq!(
            line,
            format!("Thing 3, {cut}\u{2026} \u{2014} shown in the card.")
        );
    }

    /// A block cut to its row or node budget does not show every row: no
    /// card text, at the boundary exactly.
    #[test]
    fn a_block_cut_to_its_budget_has_no_card_text() {
        let names: Vec<String> = (1..=basic::MAX_ROWS + 1).map(|i| format!("T{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(card_text(&table_card(&refs), ""), None);
        assert!(card_text(&table_card(&refs[..basic::MAX_ROWS]), "").is_some());
        let items: Vec<Value> = names.iter().map(|n| json!({ "label": n })).collect();
        let list = summary(json!({ "surface": { "components": [
            { "kind": "list", "items": items } ] } }));
        assert_eq!(card_text(&list, ""), None);
        let nodes: Vec<Value> = (0..=basic::MAX_TREE_NODES)
            .map(|i| json!({ "label": format!("N{i}") }))
            .collect();
        let tree = summary(json!({ "surface": { "components": [
            { "kind": "tree", "nodes": nodes } ] } }));
        assert_eq!(card_text(&tree, ""), None);
    }

    /// A card that lost body blocks to the size budget does not show all the
    /// text says: no card text.
    #[test]
    fn a_cut_card_has_no_card_text() {
        let mut d = table_card(&["A"]);
        d.cut = true;
        assert_eq!(card_text(&d, CARD_NOTE), None);
    }

    /// Only basic-catalog producer surfaces reach the widget (its renderer
    /// ships no other catalog), ahead of Triton's own card.
    #[test]
    fn basic_passthrough_surfaces_lead_and_others_are_left_out() {
        let c = card(&json!({ "surface": {
            "components": [ { "kind": "header", "title": "T" } ],
            "a2ui": [
                { "catalogId": passthrough::BASIC_CATALOG_ID,
                  "components": [ { "id": "root", "component": "Text", "text": "mine" } ] },
                { "catalogId": passthrough::GE_COMPOSITE_CATALOG_ID,
                  "components": [ { "id": "root", "component": "MaterialCard", "children": [] } ] }
            ]
        }}))
        .unwrap();
        let ids: Vec<&str> = c
            .messages
            .iter()
            .filter_map(|m| m["createSurface"]["surfaceId"].as_str())
            .collect();
        assert_eq!(ids.len(), 2, "{:#?}", c.messages);
        assert!(ids[0].starts_with(passthrough::SURFACE_PREFIX));
        assert_eq!(ids[1], basic::SURFACE_ID);
    }
}
