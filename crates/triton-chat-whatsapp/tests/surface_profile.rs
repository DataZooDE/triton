//! #352: the mapper's caps come from its `SurfaceProfile`, the profile Triton
//! advertises to upstream agents. These tests pin both to literal values, so a
//! refactor can't move a limit silently and the hint can't drift from what the
//! mapper actually does.

use serde_json::json;
use triton_chat_whatsapp::surface_mapper::{SURFACE, render};
use triton_core::a2ui::Surface;
use triton_core::surface::{Charts, Markdown, SurfaceProfile, Tables};

#[test]
fn surface_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "whatsapp",
        markdown: Markdown::Basic,
        tables: Tables::Text,
        buttons: true,
        max_buttons: Some(3),
        charts: Charts::None,
        images: true,
        max_text_bytes: Some(4096),
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
fn reply_text_is_capped_at_4096_bytes() {
    let long = "x".repeat(4096 + 500);
    let s: Surface =
        serde_json::from_value(json!({ "components": [{ "kind": "text", "value": long }] }))
            .expect("surface");
    let text = render(&s, b"k", "acme", "u").expect("render").text;
    assert!(
        text.len() <= 4096,
        "{} bytes exceeds the 4096-byte cap",
        text.len()
    );
    assert!(
        text.len() > 4096 / 2,
        "the text must be truncated, not dropped"
    );
}

/// Four buttons: WhatsApp shows three reply buttons, the fourth is deferred.
#[test]
fn reply_buttons_are_capped_at_three() {
    let buttons: Vec<_> = (0..4)
        .map(|i| json!({ "kind": "button", "label": format!("b{i}"), "tool": "go", "args": {} }))
        .collect();
    let mut components = vec![json!({ "kind": "text", "value": "pick one" })];
    components.extend(buttons);
    let s: Surface = serde_json::from_value(json!({ "components": components })).expect("surface");
    let r = render(&s, b"k", "acme", "u").expect("render");
    let shown = r.interactive.expect("interactive")["action"]["buttons"]
        .as_array()
        .expect("buttons")
        .len();
    assert_eq!(shown, 3);
    assert_eq!(r.deferred_buttons, 1);
}
