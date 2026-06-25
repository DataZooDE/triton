//! SVG → PNG rendering. Wraps `resvg`/`tiny-skia` so the binary
//! has one call site to consume.
//!
//! **Fonts (issue #135).** The SVG template draws all content as
//! `<text font-family="sans-serif">`. `usvg::Options::default()` starts
//! with an *empty* font database, so without a font every glyph is
//! dropped and dashboards render as empty colored boxes. We therefore
//! **embed** a sans-serif face (DejaVu Sans, `include_bytes!`) and
//! register it as the `sans-serif` family. This is fully deterministic
//! (no system-font-install step, identical output in CI and on a
//! minimal/distroless sidecar) and adds no *runtime* system dependency —
//! the font is data baked into the binary, consistent with NFR-PT-2.
//!
//! Operators who also need non-Latin scripts can opt into the host's
//! system fonts with `TRITON_RASTERIZER_LOAD_SYSTEM_FONTS=1`.

use std::sync::{Arc, OnceLock};

use resvg::tiny_skia;
use resvg::usvg;
use resvg::usvg::fontdb;
use thiserror::Error;

/// Embedded sans-serif face. DejaVu Sans (Bitstream Vera license —
/// permissive redistribution; see `assets/DejaVuSans-LICENSE.txt`).
const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
const FONT_FAMILY: &str = "DejaVu Sans";

/// Process-wide font database: the embedded face registered as the
/// `sans-serif` family, built once. Parsing the face on every render
/// would be wasteful, so we cache the `Arc` and clone it into each
/// `Options`.
fn shared_fontdb() -> Arc<fontdb::Database> {
    static DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_font_data(EMBEDDED_FONT.to_vec());
        // Opt-in: also pull in the host's system fonts (e.g. for
        // non-Latin scripts the embedded face doesn't cover).
        if load_system_fonts_opt_in() {
            db.load_system_fonts();
        }
        // The template's generic `sans-serif` resolves to the embedded face.
        db.set_sans_serif_family(FONT_FAMILY);
        Arc::new(db)
    })
    .clone()
}

fn load_system_fonts_opt_in() -> bool {
    std::env::var("TRITON_RASTERIZER_LOAD_SYSTEM_FONTS")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("SVG parse failed: {0}")]
    Parse(String),
    #[error("could not allocate {width}x{height} pixmap")]
    Pixmap { width: u32, height: u32 },
    #[error("PNG encode failed: {0}")]
    Encode(String),
}

/// Render an SVG document string to PNG bytes at the document's
/// declared size. The renderer is synchronous + CPU-bound; the
/// caller wraps it in [`tokio::time::timeout`] (see
/// `bin/triton-rasterizer.rs`).
pub fn render_png(svg: &str, width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
    let opts = usvg::Options {
        fontdb: shared_fontdb(),
        // Ultimate fallback family (used if the SVG ever names an unknown
        // family) — point it at the embedded face too, not usvg's default
        // "Times New Roman" which the empty db can't resolve.
        font_family: FONT_FAMILY.to_string(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opts).map_err(|e| RenderError::Parse(e.to_string()))?;

    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or(RenderError::Pixmap { width, height })?;

    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );

    pixmap
        .encode_png()
        .map_err(|e| RenderError::Encode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_minimal_svg_to_png() {
        // 100x80 red rect — tiniest reasonable SVG that exercises
        // the parse-render-encode path end-to-end.
        let svg = r##"<?xml version="1.0"?>
<svg xmlns="http://www.w3.org/2000/svg" width="100" height="80" viewBox="0 0 100 80">
  <rect x="0" y="0" width="100" height="80" fill="#ff0000"/>
</svg>"##;
        let png = render_png(svg, 100, 80).expect("render");
        // PNG magic bytes.
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.len() > 50, "expected non-trivial PNG body");
    }

    #[test]
    fn malformed_svg_returns_parse_error() {
        let err = render_png("not an svg", 10, 10).expect_err("should fail");
        assert!(matches!(err, RenderError::Parse(_)));
    }

    /// Count near-black opaque pixels — i.e. rendered text ink. The
    /// dashboard template draws every glyph in slate-900 (`#0f172a`); the
    /// background/tiles/borders are all light, so any dark pixel is text.
    fn dark_pixels(png: &[u8]) -> usize {
        let pm = tiny_skia::Pixmap::decode_png(png).expect("decode png");
        pm.pixels()
            .iter()
            .filter(|p| p.red() < 80 && p.green() < 80 && p.blue() < 80 && p.alpha() > 200)
            .count()
    }

    /// Issue #135: a dashboard's title/labels/values must actually render
    /// as visible glyphs. With no font loaded, usvg drops the `<text>`
    /// content and the PNG is empty colored boxes — so there are ~zero
    /// dark (text-ink) pixels. With a font, the glyphs paint slate-900 ink.
    #[test]
    fn dashboard_text_renders_visible_glyphs() {
        use crate::DashboardRequest;
        use triton_core::a2ui::DashboardTile;

        let req = DashboardRequest {
            title: "Supplier Risk".into(),
            tiles: vec![DashboardTile {
                label: "Score".into(),
                value: "8 8 8 8 8".into(),
                trend: Some("+12%".into()),
            }],
        };
        let (svg, height) = crate::svg::build(&req);
        let png = render_png(&svg, 1200, height).expect("render");

        let dark = dark_pixels(&png);
        assert!(
            dark > 300,
            "dashboard text must render as visible glyphs (#135); found only {dark} \
             dark text-ink pixels — the font database is empty so `<text>` was dropped",
        );
    }
}
