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

    /// JSON Schema for the tool's argument shape. Backs `GET /v1/tools`
    /// per FR-A-5. The default is the permissive "any object", which
    /// makes the trait friendly for stubs and tests — production
    /// tools MUST override with a real schema.
    ///
    /// **Public surface.** Whatever `input_schema()` returns is
    /// serialised verbatim to every authenticated `/v1/tools`
    /// caller. Don't include secret names, internal provider
    /// details, tenant-specific configuration, or anything else you
    /// wouldn't post on the API reference page; tools that need
    /// per-tenant variations should branch at `invoke()` time, not
    /// in the published schema.
    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    /// Whether the tool's success result is an A2UI envelope (so REST
    /// can advertise the right `application/json+a2ui` content-type
    /// per FR-A-5). Default `false`; tools that emit a surface
    /// override.
    fn returns_a2ui(&self) -> bool {
        false
    }

    /// Validate and execute. Tools see a redacted `ToolPrincipal` —
    /// they cannot read the inbound bearer token, by type. The
    /// upstream-router crate (PR 9) gets `raw_token` access through
    /// the dispatcher's full `Principal`, not via this trait.
    async fn invoke(&self, args: Value, principal: &ToolPrincipal) -> Result<Value, TritonError>;
}

/// Public view of a registered tool, surfaced by `GET /v1/tools`
/// (FR-A-5). Cheap to clone.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub input_schema: Value,
    pub returns_a2ui: bool,
    /// `true` for tools that aren't in the in-process registry but are
    /// reachable via the upstream router (a Consul `agent:<name>`
    /// service). Triton doesn't know their input schema, so `input_schema`
    /// is empty and `returns_a2ui` is assumed `true` (agents typically
    /// emit surfaces). Lets the Explorer tag them and still call them.
    #[serde(default)]
    pub upstream: bool,
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

    /// Sorted list of public descriptors for every registered tool —
    /// the shape `GET /v1/tools` returns.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|t| ToolDescriptor {
                name: t.name().to_string(),
                input_schema: t.input_schema(),
                returns_a2ui: t.returns_a2ui(),
                upstream: false,
            })
            .collect()
    }
}
