//! Pure-function SVG template builder.
//!
//! Inputs: validated [`DashboardRequest`] (caller already enforced
//! [`MAX_TILES`] / [`MAX_TITLE_BYTES`]).
//!
//! Output: an SVG document string consumable by `usvg::Tree::from_str`.
//! Deliberately boring — fixed 1200-wide canvas, 4-column grid, no
//! embedded fonts (we configure usvg with a fallback font database
//! so any reasonable Linux substrate node renders the same shape).
//!
//! Why a template-string approach rather than an SVG-builder DSL:
//! a string template with explicit XML escaping reviewer-readable.
//! The XML produced is small enough (kilobytes) that string
//! concatenation isn't a performance concern; the bottleneck is
//! the raster step, not SVG construction.

use crate::DashboardRequest;
use triton_core::a2ui::DashboardTile;

const CANVAS_WIDTH: u32 = 1200;
const COLUMNS: usize = 4;
const TILE_W: u32 = 270;
const TILE_H: u32 = 140;
const TILE_GAP: u32 = 20;
const MARGIN: u32 = 40;
const TITLE_HEIGHT: u32 = 80;
const BG_COLOR: &str = "#f8fafc"; // slate-50 — neutral, prints OK
const TILE_BG: &str = "#ffffff";
const TILE_BORDER: &str = "#cbd5e1"; // slate-300
const TITLE_COLOR: &str = "#0f172a"; // slate-900
const LABEL_COLOR: &str = "#475569"; // slate-600
const VALUE_COLOR: &str = "#0f172a";
const TREND_COLOR: &str = "#475569";

/// Build the SVG document for `req`. Returns the canvas height too
/// — the renderer needs it to allocate the pixmap. Height grows
/// with tile count so a many-tile dashboard isn't clipped.
pub fn build(req: &DashboardRequest) -> (String, u32) {
    let rows = req.tiles.len().div_ceil(COLUMNS).max(1);
    let height: u32 = MARGIN
        + TITLE_HEIGHT
        + (rows as u32) * TILE_H
        + (rows.saturating_sub(1) as u32) * TILE_GAP
        + MARGIN;

    let mut s = String::new();
    s.push_str(&format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS_WIDTH}" height="{height}" viewBox="0 0 {CANVAS_WIDTH} {height}">
"#
    ));
    s.push_str(&format!(
        r#"  <rect x="0" y="0" width="{CANVAS_WIDTH}" height="{height}" fill="{BG_COLOR}"/>
"#
    ));
    s.push_str(&format!(
        r#"  <text x="{MARGIN}" y="{title_y}" font-family="sans-serif" font-size="36" font-weight="700" fill="{TITLE_COLOR}">{title}</text>
"#,
        title_y = MARGIN + 40,
        title = xml_escape(&req.title),
    ));

    for (idx, tile) in req.tiles.iter().enumerate() {
        let col = idx % COLUMNS;
        let row = idx / COLUMNS;
        let x = MARGIN + (col as u32) * (TILE_W + TILE_GAP);
        let y = MARGIN + TITLE_HEIGHT + (row as u32) * (TILE_H + TILE_GAP);
        push_tile(&mut s, x, y, tile);
    }

    s.push_str("</svg>\n");
    (s, height)
}

fn push_tile(out: &mut String, x: u32, y: u32, tile: &DashboardTile) {
    out.push_str(&format!(
        r#"  <rect x="{x}" y="{y}" width="{TILE_W}" height="{TILE_H}" rx="12" ry="12" fill="{TILE_BG}" stroke="{TILE_BORDER}" stroke-width="1"/>
"#
    ));
    let label_y = y + 32;
    let value_y = y + 84;
    let trend_y = y + 118;
    out.push_str(&format!(
        r#"  <text x="{tx}" y="{label_y}" font-family="sans-serif" font-size="16" fill="{LABEL_COLOR}">{label}</text>
"#,
        tx = x + 20,
        label = xml_escape(&tile.label),
    ));
    out.push_str(&format!(
        r#"  <text x="{tx}" y="{value_y}" font-family="sans-serif" font-size="32" font-weight="700" fill="{VALUE_COLOR}">{value}</text>
"#,
        tx = x + 20,
        value = xml_escape(&tile.value),
    ));
    if let Some(trend) = tile.trend.as_deref() {
        out.push_str(&format!(
            r#"  <text x="{tx}" y="{trend_y}" font-family="sans-serif" font-size="14" fill="{TREND_COLOR}">{trend}</text>
"#,
            tx = x + 20,
            trend = xml_escape(trend),
        ));
    }
}

/// XML attribute / text-node escape. Tool output is untrusted —
/// a tile value containing `</text>` would break the document
/// without this, and a value containing `&` would render literally
/// instead of escaping the entity reference.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use triton_core::a2ui::DashboardTile;

    #[test]
    fn empty_tiles_still_emits_canvas_and_title() {
        let req = DashboardRequest {
            title: "Empty".into(),
            tiles: vec![],
        };
        let (svg, h) = build(&req);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("Empty"));
        assert!(h > 0);
    }

    #[test]
    fn xml_special_chars_in_tiles_are_escaped() {
        let req = DashboardRequest {
            title: "<title>".into(),
            tiles: vec![DashboardTile {
                label: "p&q".into(),
                value: "</text>".into(),
                trend: Some("a>b".into()),
            }],
        };
        let (svg, _) = build(&req);
        // Tool input must never break out of the text node.
        assert!(!svg.contains("<title>") || svg.contains("&lt;title&gt;"));
        assert!(svg.contains("&lt;/text&gt;"));
        assert!(svg.contains("p&amp;q"));
        assert!(svg.contains("a&gt;b"));
    }

    #[test]
    fn grid_layout_uses_four_columns() {
        let req = DashboardRequest {
            title: "Grid".into(),
            tiles: (0..8)
                .map(|i| DashboardTile {
                    label: format!("t{i}"),
                    value: format!("{i}"),
                    trend: None,
                })
                .collect(),
        };
        let (svg, _h) = build(&req);
        // 8 tiles = 2 rows of 4. Count rect tiles (rounded
        // rect with rx="12") to confirm one per tile.
        let tile_rects = svg.matches(r#"rx="12""#).count();
        assert_eq!(tile_rects, 8);
    }
}
