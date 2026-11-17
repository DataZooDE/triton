//! MCP face of in-card actions (#369): the app-only tool and its argument
//! shape. The checks live in `Dispatcher::invoke_in_card`, the token format
//! in `triton_core::a2ui::in_card`; this module only wraps and unwraps.

use serde_json::{Map, Value, json};
use triton_core::a2ui::ge::ActionSigner;
use triton_core::a2ui::in_card::{self, Holder, Scope};
use triton_core::dispatcher::InCardEvent;

/// The `tools/list` entry for [`in_card::TOOL_NAME`]: hidden from the model
/// (`_meta.ui.visibility: ["app"]`, MCP Apps), callable by the widget
/// (`openai/widgetAccessible`, ChatGPT's older spelling), read-only.
pub fn descriptor() -> Value {
    json!({
        "name": in_card::TOOL_NAME,
        "title": "Card interaction",
        "description": "Updates the answer card in place (paging, filters, refresh). \
                        Called by the card, not by the model.",
        "inputSchema": {
            "type": "object",
            "required": ["surfaceId", "name", "token"],
            "properties": {
                "surfaceId": { "type": "string" },
                "name": { "type": "string" },
                "sourceComponentId": { "type": "string" },
                "context": { "type": "object" },
                "token": { "type": "string" }
            }
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false },
        "_meta": {
            "ui": { "visibility": ["app"] },
            "openai/widgetAccessible": true,
            "openai/visibility": "private"
        }
    })
}

/// The widget's arguments as an [`InCardEvent`]. Missing strings are empty
/// and a missing context is `{}`; the dispatcher decides what that means.
pub fn event(args: &Value) -> InCardEvent {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    InCardEvent {
        surface_id: s("surfaceId"),
        name: s("name"),
        source_component_id: s("sourceComponentId"),
        context: match args.get("context") {
            None | Some(Value::Null) => json!({}),
            Some(c) => c.clone(),
        },
        token: args
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Who may redeem the controls of a card, and which call they repeat.
pub struct Origin<'a> {
    pub tool: &'a str,
    /// The call's own arguments (as dispatched, without `_triton`).
    pub args: &'a Value,
    pub holder: Holder<'a>,
}

impl Origin<'_> {
    /// A rerun control's token: this call's arguments with the control's
    /// laid over them.
    pub fn sign_rerun(&self, overlay: &Map<String, Value>) -> Option<String> {
        in_card::sign(
            ActionSigner::global(),
            self.tool,
            &Scope::Rerun,
            &in_card::merge_args(self.args, overlay)?,
            self.holder,
        )
    }

    /// The token for the producer surfaces' in-card events among `messages`
    /// (the widget's): this call, and exactly those event names on exactly
    /// those surfaces. `None` when no producer surface fires one.
    pub fn sign_events(&self, messages: &[Value]) -> Option<String> {
        let (names, surfaces) = producer_events(messages);
        if names.is_empty() {
            return None;
        }
        in_card::sign(
            ActionSigner::global(),
            self.tool,
            &Scope::Event { names, surfaces },
            &in_card::merge_args(self.args, &Map::new())?,
            self.holder,
        )
    }
}

/// The in-card event names on producer surfaces (not Triton's card), and
/// the ids of the surfaces that fire them.
fn producer_events(messages: &[Value]) -> (Vec<String>, Vec<String>) {
    let (mut names, mut surfaces) = (Vec::<String>::new(), Vec::<String>::new());
    for m in messages {
        let Some(u) = m.get("updateComponents") else {
            continue;
        };
        let Some(id) = u.get("surfaceId").and_then(Value::as_str) else {
            continue;
        };
        if id == triton_core::a2ui::basic::SURFACE_ID {
            continue;
        }
        let found = in_card::event_names(u.get("components").unwrap_or(&Value::Null));
        if found.is_empty() {
            continue;
        }
        if !surfaces.iter().any(|s| s == id) {
            surfaces.push(id.to_string());
        }
        for n in found {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    (names, surfaces)
}
