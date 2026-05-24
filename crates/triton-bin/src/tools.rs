//! In-process tools registered for the walking skeleton. The
//! `echo` tool just round-trips its argument so PR 4 can prove the
//! dispatcher + audit pipeline end-to-end before the real upstream
//! router (PR 9) replaces in-process tools with Consul-discovered
//! agent calls.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
            "properties": {
                "message": { "type": "string" }
            },
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
