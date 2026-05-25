//! SVG → PNG rendering. Wraps `resvg`/`tiny-skia` so the binary
//! has one call site to consume.
//!
//! The default `usvg::Options` initialises a font database lazily;
//! we DON'T load system fonts (NFR-PT-2 — no system deps for the
//! static link) but we DO accept the absence by letting usvg fall
//! through to its missing-glyph behaviour. Text labels are
//! plain-ASCII enough in the SVG template that any sans-serif
//! fallback renders intelligibly; on a substrate node missing all
//! fonts we still produce a valid PNG, just with placeholder
//! glyphs for the text content. This is the simplest path that
//! makes CI deterministic (no font-package-install step).

use resvg::tiny_skia;
use resvg::usvg;
use thiserror::Error;

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
    let opts = usvg::Options::default();
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
}
