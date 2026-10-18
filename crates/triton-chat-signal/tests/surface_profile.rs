//! #352: the mapper's caps come from its `SurfaceProfile`, the profile Triton
//! advertises to upstream agents. These tests pin both to literal values, so a
//! refactor can't move a limit silently and the hint can't drift from what the
//! mapper actually does.

use serde_json::json;
use triton_chat_signal::surface_mapper::{SURFACE, render};
use triton_core::a2ui::Surface;
use triton_core::surface::{Charts, Markdown, SurfaceProfile, Tables};

#[test]
fn surface_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "signal",
        markdown: Markdown::None,
        tables: Tables::Text,
        buttons: false,
        max_buttons: None,
        charts: Charts::None,
        images: false,
        max_text_bytes: Some(2000),
        streaming: false,
        subject_line: false,
        guidance: SURFACE.guidance,
    };
    assert_eq!(SURFACE, expected);
    assert!(
        !SURFACE.guidance.is_empty(),
        "every profile ships default guidance"
    );
}

/// The text cap the mapper enforces is the literal one the profile declares.
#[test]
fn reply_text_is_capped_at_2000_bytes() {
    let long = "x".repeat(2000 + 500);
    let s: Surface =
        serde_json::from_value(json!({ "components": [{ "kind": "text", "value": long }] }))
            .expect("surface");
    let text = render(&s).expect("render").text;
    assert!(
        text.len() <= 2000,
        "{} bytes exceeds the 2000-byte cap",
        text.len()
    );
    assert!(
        text.len() > 2000 / 2,
        "the text must be truncated, not dropped"
    );
}
