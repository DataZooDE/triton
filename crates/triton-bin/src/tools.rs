//! In-process tools registered for the walking skeleton.
//!
//! * `echo` — round-trips its argument so PR 4 could prove the
//!   dispatcher + audit pipeline end-to-end.
//! * `narrate` — emits a small A2UI surface so PR 10 can exercise
//!   the v0.8/v0.9 builders + ACC-1 parity across the HTTP trio.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use triton_core::a2ui::{Component, Surface};
use triton_core::{Tool, ToolPrincipal, TritonError};

pub struct Echo;

#[derive(Debug, Deserialize)]
struct EchoArgs {
    message: String,
}

#[derive(Debug, Serialize)]
struct EchoOut {
    echo: String,
}

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": { "message": { "type": "string" } },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: EchoArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("echo args: {e}")))?;
        let out = EchoOut {
            echo: parsed.message,
        };
        Ok(serde_json::to_value(out).expect("EchoOut serialises"))
    }
}

pub struct Narrate;

#[derive(Debug, Deserialize)]
struct NarrateArgs {
    subject: String,
}

#[async_trait]
impl Tool for Narrate {
    fn name(&self) -> &'static str {
        "narrate"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["subject"],
            "properties": { "subject": { "type": "string" } },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: NarrateArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("narrate args: {e}")))?;
        // The tool builds a canonical, version-agnostic Surface.
        // The adapter wraps it with the negotiated builder
        // (ADR-4: dispatcher and tools never branch on version).
        let surface = Surface {
            components: vec![
                Component::Text {
                    value: format!("Hello, {}.", parsed.subject),
                },
                Component::Narration {
                    text: format!("This is a generated narration about {}.", parsed.subject),
                },
                Component::Button {
                    label: "Refresh".into(),
                    tool: "narrate".into(),
                    args: serde_json::json!({ "subject": parsed.subject }),
                },
            ],
        };
        Ok(serde_json::json!({
            "surface": surface,
        }))
    }
}
