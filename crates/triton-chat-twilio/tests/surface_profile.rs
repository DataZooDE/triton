//! #352: the mapper's caps come from its `SurfaceProfile`, the profile Triton
//! advertises to upstream agents. These tests pin both to literal values, so a
//! refactor can't move a limit silently and the hint can't drift from what the
//! mapper actually does.

use serde_json::json;
use triton_chat_twilio::surface_mapper::{SURFACE_RCS, SURFACE_WHATSAPP, render};
use triton_core::a2ui::Surface;
use triton_core::surface::{Charts, Markdown, SurfaceProfile, Tables};

#[test]
fn surface_whatsapp_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "twilio_whatsapp",
        markdown: Markdown::Basic,
        tables: Tables::Text,
        buttons: false,
        max_buttons: None,
        charts: Charts::None,
        images: false,
        max_text_bytes: Some(1600),
        streaming: false,
        subject_line: false,
        guidance: SURFACE_WHATSAPP.guidance,
    };
    assert_eq!(SURFACE_WHATSAPP, expected);
    assert!(
        !SURFACE_WHATSAPP.guidance.is_empty(),
        "every profile ships default guidance"
    );
}

#[test]
fn surface_rcs_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "twilio_rcs",
        markdown: Markdown::Basic,
        tables: Tables::Text,
        buttons: false,
        max_buttons: None,
        charts: Charts::None,
        images: false,
        max_text_bytes: Some(1600),
        streaming: false,
        subject_line: false,
        guidance: SURFACE_RCS.guidance,
    };
    assert_eq!(SURFACE_RCS, expected);
    assert!(
        !SURFACE_RCS.guidance.is_empty(),
        "every profile ships default guidance"
    );
}

/// The text cap the mapper enforces is the literal one the profile declares.
#[test]
fn reply_text_is_capped_at_1600_bytes() {
    let long = "x".repeat(1600 + 500);
    let s: Surface =
        serde_json::from_value(json!({ "components": [{ "kind": "text", "value": long }] }))
            .expect("surface");
    let text = render(&s).expect("render").text;
    assert!(
        text.len() <= 1600,
        "{} bytes exceeds the 1600-byte cap",
        text.len()
    );
    assert!(
        text.len() > 1600 / 2,
        "the text must be truncated, not dropped"
    );
}
