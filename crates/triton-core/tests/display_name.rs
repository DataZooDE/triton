//! The deployment's default surface `agentDisplayName`
//! (`TRITON_A2UI_AGENT_DISPLAY_NAME`). A process-wide setting installed once
//! at boot, so it is tested in its own test binary: the unit tests of the
//! crate keep the built-in default.

use serde_json::{Value, json};
use triton_core::a2ui::{ge, passthrough};

fn theme_names(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| m.pointer("/createSurface/theme/agentDisplayName"))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_configured_display_name_names_every_surface_triton_builds() {
    // Malformed values are refused and install nothing.
    for bad in ["", "   ", "a\u{7}b", &"x".repeat(65)] {
        assert!(
            passthrough::configure_display_name(Some(bad)).is_err(),
            "{bad:?}"
        );
    }
    // Unset keeps the default.
    passthrough::configure_display_name(None).expect("unset is fine");
    let card = json!({ "summary": "s", "surface": { "components": [
        { "kind": "header", "title": "Three items" },
        { "kind": "button", "label": "More",
          "action": { "type": "send_message", "text": "Show more" } }
    ] } });
    let before = ge::build_messages(&card).expect("messages");
    assert_eq!(
        theme_names(&before),
        vec![passthrough::DEFAULT_DISPLAY_NAME]
    );

    passthrough::configure_display_name(Some("  Portfolio Agent ")).expect("valid");
    // Installed once: a second call keeps the first name.
    passthrough::configure_display_name(Some("Other")).expect("valid");

    let own = ge::build_messages(&card).expect("messages");
    assert_eq!(theme_names(&own), vec!["Portfolio Agent"]);
    let canvas = ge::build_document_canvas("https://docs.example/d/1", "Doc");
    assert_eq!(theme_names(&canvas), vec!["Portfolio Agent"]);
    let material = ge::build_document_material(&json!({}), "Doc");
    assert_eq!(theme_names(&material), vec!["Portfolio Agent"]);
}
