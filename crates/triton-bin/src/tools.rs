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

/// Synchronously sleep for the requested number of milliseconds.
/// Used by ACC-2's mid-flight-SIGTERM test. **Dev-only**: gated by
/// the `dev-token` cargo feature so production builds
/// (`--no-default-features`) don't ship a tool that lets any
/// authenticated caller park a request task for `u64::MAX` ms.
#[cfg(feature = "dev-token")]
pub struct Delay;

/// Cap on `delay.ms` even in dev. The test uses 1500 ms; raising
/// the cap doesn't help anyone and a runaway value (`u64::MAX`)
/// would tie up a tokio task essentially forever.
#[cfg(feature = "dev-token")]
const DELAY_MAX_MS: u64 = 5_000;

#[cfg(feature = "dev-token")]
#[derive(Debug, Deserialize)]
struct DelayArgs {
    ms: u64,
}

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for Delay {
    fn name(&self) -> &'static str {
        "delay"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["ms"],
            "properties": {
                "ms": { "type": "integer", "minimum": 0, "maximum": DELAY_MAX_MS }
            },
            "additionalProperties": false
        })
    }

    async fn invoke(&self, args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let parsed: DelayArgs = serde_json::from_value(args)
            .map_err(|e| TritonError::Validation(format!("delay args: {e}")))?;
        if parsed.ms > DELAY_MAX_MS {
            return Err(TritonError::Validation(format!(
                "delay.ms must be <= {DELAY_MAX_MS}"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(parsed.ms)).await;
        Ok(serde_json::json!({ "delayed_ms": parsed.ms }))
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

/// Dev-only A2UI tool emitting an *empty* Surface so the PR 20
/// integration test can drive the L6'-edge empty-surface fallback
/// end-to-end. Without this, the only path to an empty Surface
/// would be a misbehaving production tool — which the
/// `dev-token`-gated registry entry below makes impossible to
/// register in a production build (ADR-10 / FR-I-5 parallel).
#[cfg(feature = "dev-token")]
pub struct EmptySurface;

#[cfg(feature = "dev-token")]
#[async_trait]
impl Tool for EmptySurface {
    fn name(&self) -> &'static str {
        "empty_surface"
    }

    fn returns_a2ui(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false
        })
    }

    async fn invoke(&self, _args: Value, _principal: &ToolPrincipal) -> Result<Value, TritonError> {
        let surface = Surface { components: vec![] };
        Ok(serde_json::json!({ "surface": surface }))
    }
}
