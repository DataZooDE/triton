//! The `Tool` trait and `ToolRegistry`. A tool is the user-facing
//! verb (`echo`, `compute_stats`, ...); the registry is the single
//! lookup the dispatcher uses on every invocation (FR-D-1).
//!
//! PR 4 has tools live in-process (the trivial `echo`). PR 9 swaps
//! the implementation for an upstream-router-backed tool that does
//! Consul lookup + Vault token mint + HTTP/1.1 dispatch over the
//! tailnet. Adapters and the dispatcher do not care which variant
//! is registered — that's the point of the trait (open/closed).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::TritonError;
use crate::principal::ToolPrincipal;

#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable lookup name; matches the wire path `/v1/tools/<name>`.
    fn name(&self) -> &'static str;

    /// Validate and execute. Tools see a redacted `ToolPrincipal` —
    /// they cannot read the inbound bearer token, by type. The
    /// upstream-router crate (PR 9) gets `raw_token` access through
    /// the dispatcher's full `Principal`, not via this trait.
    async fn invoke(&self, args: Value, principal: &ToolPrincipal) -> Result<Value, TritonError>;
}

/// Registry of tools the dispatcher knows about. Sorted by name so
/// `GET /v1/tools` (PR 5) returns a stable order.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Returns the previous entry if one was
    /// registered under the same name — useful for tests; in `main`
    /// every name is unique.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        self.tools.insert(tool.name(), tool)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.tools.keys().copied()
    }
}
