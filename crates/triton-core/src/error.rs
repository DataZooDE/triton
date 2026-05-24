//! The four typed errors the dispatcher surfaces (FR-D-4). Each
//! adapter MUST translate the variant — not the inner message —
//! into the protocol-appropriate response (architecture.md §8.3).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TritonError {
    /// Identity check failed before the dispatcher could be reached
    /// (FR-I-1). Adapter maps to HTTP 401 / JSON-RPC -32001 / A2A
    /// `metadata.error: "auth"`.
    #[error("auth: {0}")]
    Auth(String),

    /// Argument schema validation failed (FR-D-2). HTTP 400 /
    /// JSON-RPC -32602 / A2A `metadata.error: "validation"`.
    #[error("validation: {0}")]
    Validation(String),

    /// The handler ran but produced an error (timeout, open
    /// circuit-breaker, upstream agent 4xx/5xx). HTTP 502 / 503 /
    /// 504 depending on cause / JSON-RPC -32000 / A2A
    /// `metadata.error: "tool"`.
    #[error("tool: {0}")]
    Tool(String),

    /// The handler could not run because a backing service failed
    /// (Vault unreachable, Consul empty for this tool, JWKS down).
    /// HTTP 502 / JSON-RPC -32000 / A2A `metadata.error: "provider"`.
    #[error("provider: {0}")]
    Provider(String),
}

impl TritonError {
    /// Short class label used in audit lines (FR-AU-2 `result` field).
    pub fn class(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth",
            Self::Validation(_) => "validation",
            Self::Tool(_) => "tool",
            Self::Provider(_) => "provider",
        }
    }

    /// True when this is a `Tool` error whose reason is the
    /// per-tool circuit breaker being open. Architecture §8.3:
    /// REST 503 / A2A `metadata.error: "tool"` / MCP -32000. The
    /// upstream router signals this by formatting the message
    /// with the literal prefix `circuit_open`.
    pub fn is_circuit_open(&self) -> bool {
        matches!(self, Self::Tool(m) if m.starts_with("circuit_open"))
    }

    /// True when this is a `Tool` error caused by an upstream
    /// timeout (architecture §8.3: REST 504). The upstream router
    /// formats these messages with the literal suffix `timed out`.
    pub fn is_tool_timeout(&self) -> bool {
        matches!(self, Self::Tool(m) if m.contains("timed out"))
    }
}
