//! #352: the mapper's caps come from its `SurfaceProfile`, the profile Triton
//! advertises to upstream agents. These tests pin both to literal values, so a
//! refactor can't move a limit silently and the hint can't drift from what the
//! mapper actually does.

use serde_json::json;
use triton_chat_discord::surface_mapper::{SURFACE, render};
use triton_core::a2ui::Surface;
use triton_core::surface::{Charts, Markdown, SurfaceProfile, Tables};

#[test]
fn surface_profile_is_pinned() {
    let expected = SurfaceProfile {
        channel: "discord",
        markdown: Markdown::None,
        tables: Tables::Text,
        buttons: true,
        max_buttons: Some(25),
        charts: Charts::None,
        images: true,
        max_text_bytes: 2000,
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
    let text = render(&s, b"k", "acme", "u").expect("render").content;
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

/// Thirty buttons: Discord shows twenty-five (five rows of five).
#[test]
fn buttons_are_capped_at_twenty_five() {
    let buttons: Vec<_> = (0..30)
        .map(|i| json!({ "kind": "button", "label": format!("b{i}"), "tool": "go", "args": {} }))
        .collect();
    let mut components = vec![json!({ "kind": "text", "value": "pick one" })];
    components.extend(buttons);
    let s: Surface = serde_json::from_value(json!({ "components": components })).expect("surface");
    let r = render(&s, b"k", "acme", "u").expect("render");
    let shown: usize = r
        .components
        .expect("components")
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["components"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(shown, 25);
}
