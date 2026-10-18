//! #352: the mapper's caps come from its `SurfaceProfile`, the profile Triton
//! advertises to upstream agents. These tests pin both to literal values, so a
//! refactor can't move a limit silently and the hint can't drift from what the
//! mapper actually does.

use serde_json::json;
use triton_chat_email::surface_mapper::{SURFACE, render};
use triton_core::a2ui::Surface;
use triton_core::surface::{Charts, Markdown, SurfaceProfile, Tables};

#[test]
fn surface_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "email",
        markdown: Markdown::Full,
        tables: Tables::Native,
        buttons: true,
        max_buttons: None,
        charts: Charts::Image,
        images: true,
        max_text_bytes: Some(512000),
        streaming: false,
        subject_line: true,
        guidance: SURFACE.guidance,
    };
    assert_eq!(SURFACE, expected);
    assert!(
        !SURFACE.guidance.is_empty(),
        "every profile ships default guidance"
    );
}

/// The text cap the mapper enforces is the literal one the profile declares.
/// Email caps the message *body*; the HTML document chrome (≈400 bytes) is
/// wrapped around it afterwards, as the crate's own unit tests allow.
#[test]
fn reply_text_is_capped_at_512000_bytes() {
    let long = "x".repeat(512000 + 500);
    let s: Surface =
        serde_json::from_value(json!({ "components": [{ "kind": "text", "value": long }] }))
            .expect("surface");
    let text = render(&s).expect("render").html;
    assert!(
        text.len() <= 512000 + 512,
        "{} bytes exceeds the 512000-byte body cap plus chrome",
        text.len()
    );
    assert!(
        text.len() > 512000 / 2,
        "the text must be truncated, not dropped"
    );
}
