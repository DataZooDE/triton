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

    /// PR 24: the per-adapter token bucket rejected this inbound.
    /// Adapter maps to HTTP 429 Too Many Requests. The audit class
    /// `ratelimit` is distinct from `auth` so rate-limit hits show
    /// up separately in dashboards — they're not failed
    /// authentication, they're successful authentication followed
    /// by a quota refusal.
    #[error("ratelimit: {0}")]
    RateLimited(String),
}

impl TritonError {
    /// Short class label used in audit lines (FR-AU-2 `result` field).
    pub fn class(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth",
            Self::Validation(_) => "validation",
            Self::Tool(_) => "tool",
            Self::Provider(_) => "provider",
            Self::RateLimited(_) => "ratelimit",
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

    /// Canonical HTTP status for this error (architecture §8.3).
    /// Single source of truth so REST, A2A, and the dispatcher audit
    /// line agree — a `circuit_open` is 503 and an upstream timeout
    /// is 504 on every surface, not just REST.
    pub fn http_status(&self) -> u16 {
        if self.is_circuit_open() {
            return 503;
        }
        if self.is_tool_timeout() {
            return 504;
        }
        match self {
            Self::Auth(_) => 401,
            Self::Validation(_) => 400,
            Self::Tool(_) | Self::Provider(_) => 502,
            Self::RateLimited(_) => 429,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TritonError;

    #[test]
    fn http_status_mapping_matches_architecture_8_3() {
        assert_eq!(TritonError::Auth("x".into()).http_status(), 401);
        assert_eq!(TritonError::Validation("x".into()).http_status(), 400);
        assert_eq!(TritonError::Provider("x".into()).http_status(), 502);
        assert_eq!(TritonError::RateLimited("x".into()).http_status(), 429);
        // A plain tool error is 502; the circuit/timeout sentinels
        // (matched on the message) override to 503/504.
        assert_eq!(TritonError::Tool("boom".into()).http_status(), 502);
        assert_eq!(
            TritonError::Tool("circuit_open for foo".into()).http_status(),
            503
        );
        assert_eq!(
            TritonError::Tool("upstream foo timed out".into()).http_status(),
            504
        );
    }
}
