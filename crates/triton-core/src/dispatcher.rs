//! The central `invoke()` path. Every adapter (HTTP trio + future
//! chat-channel ring) funnels through this function so audit
//! symmetry is by construction, not by retrofit (ADR-6).
//!
//! Three entry points, all emitting exactly one audit line:
//!   * [`Dispatcher::invoke_with_bytes`] — raw body bytes (the
//!     normal HTTP path); a JSON-parse failure surfaces as
//!     `TritonError::Validation` with an audit line.
//!   * [`Dispatcher::invoke`] — pre-parsed `Value` body (the A2A
//!     and MCP path, which deserialise their own envelopes upstream).
//!   * [`Dispatcher::record_rejection`] — the boundary-failure
//!     path (auth, signature). Per ADR-15, an inbound rejected at
//!     auth produces a `phase: rejected` audit line *before* the
//!     dispatcher would normally run; we still own the schema.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::audit::{AuditPhase, AuditRecord, emit, now_rfc3339};
use crate::error::TritonError;
use crate::principal::Principal;
use crate::tool::ToolRegistry;

#[derive(Debug)]
pub struct Dispatch {
    pub result: Value,
    pub trace_id: String,
    pub latency_ms: u64,
}

pub struct Dispatcher {
    registry: Arc<ToolRegistry>,
    env: String,
}

impl Dispatcher {
    pub fn new(registry: Arc<ToolRegistry>, env: impl Into<String>) -> Self {
        Self {
            registry,
            env: env.into(),
        }
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// Top-level entry point for adapters that receive a raw body
    /// (REST). Parses the body into a `Value`, then dispatches.
    /// Either path produces exactly one audit line.
    pub async fn invoke_with_bytes(
        &self,
        tool_name: &str,
        body: &[u8],
        principal: Principal,
        protocol: &str,
    ) -> Result<Dispatch, TritonError> {
        // Empty body is a common idiom for no-args tools; treat as
        // `{}` to avoid forcing every client to send `{}` explicitly.
        let args: Value = if body.is_empty() {
            Value::Object(Default::default())
        } else {
            match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => {
                    let err = TritonError::Validation(format!("invalid JSON body: {e}"));
                    return Err(self.fail(tool_name, protocol, &principal, &err, 0));
                }
            }
        };
        self.invoke(tool_name, args, principal, protocol).await
    }

    /// Dispatch a tool call given pre-parsed args. Emits one audit
    /// line and translates panics inside the tool to
    /// `TritonError::Tool` so the audit guarantee holds.
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: Value,
        principal: Principal,
        protocol: &str,
    ) -> Result<Dispatch, TritonError> {
        let started = Instant::now();
        let outcome = self.run(tool_name, args, &principal).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        self.audit_dispatch(tool_name, protocol, &principal, latency_ms, &outcome);
        outcome.map(|result| Dispatch {
            result,
            trace_id: principal.trace_id,
            latency_ms,
        })
    }

    /// Emit a `phase: rejected` audit line for an inbound the
    /// adapter declined before the dispatcher could run (auth,
    /// signature). Schema construction stays here so adapters
    /// remain audit-emission-free (ADR-6).
    ///
    /// `tool_name` is the route segment the caller targeted; on a
    /// missing/malformed Authorization header the adapter knows the
    /// route but has no Principal — pass synthetic `subject = "-"`,
    /// `tenant = "-"`, and a fresh `trace_id` to keep the schema
    /// uniform.
    pub fn record_rejection(
        &self,
        tool_name: &str,
        protocol: &str,
        subject: &str,
        tenant: &str,
        trace_id: &str,
        error: &TritonError,
    ) {
        let status = status_for(error);
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Rejected,
            when: now_rfc3339(),
            who: subject,
            what: tool_name,
            env: &self.env,
            result: format!("error:{}", error.class()),
            protocol,
            tool: tool_name,
            subject,
            tenant,
            latency_ms: 0,
            status,
            trace_id,
        });
    }

    async fn run(
        &self,
        tool_name: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        let tool = self
            .registry
            .get(tool_name)
            .ok_or_else(|| TritonError::Validation(format!("unknown tool: {tool_name}")))?;

        // Spawn so a panic inside the tool becomes a JoinError we
        // can translate to TritonError::Tool — that keeps the
        // "one audit line per invocation" guarantee even when a
        // tool implementer screws up. The cost is one extra task
        // hop per dispatch (μs scale) and a clone of the redacted
        // principal; both acceptable.
        let tp = principal.to_tool_principal();
        let join = tokio::spawn(async move { tool.invoke(args, &tp).await });
        match join.await {
            Ok(result) => result,
            Err(join_err) => {
                let what = if join_err.is_panic() {
                    "panic"
                } else if join_err.is_cancelled() {
                    "cancelled"
                } else {
                    "join_error"
                };
                Err(TritonError::Tool(format!("tool {what}: {join_err}")))
            }
        }
    }

    fn fail(
        &self,
        tool_name: &str,
        protocol: &str,
        principal: &Principal,
        error: &TritonError,
        latency_ms: u64,
    ) -> TritonError {
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Dispatch,
            when: now_rfc3339(),
            who: &principal.sub,
            what: tool_name,
            env: &self.env,
            result: format!("error:{}", error.class()),
            protocol,
            tool: tool_name,
            subject: &principal.sub,
            tenant: &principal.tenant,
            latency_ms,
            status: status_for(error),
            trace_id: &principal.trace_id,
        });
        // Reconstruct a parallel error so we can both audit and return.
        match error {
            TritonError::Auth(m) => TritonError::Auth(m.clone()),
            TritonError::Validation(m) => TritonError::Validation(m.clone()),
            TritonError::Tool(m) => TritonError::Tool(m.clone()),
            TritonError::Provider(m) => TritonError::Provider(m.clone()),
        }
    }

    fn audit_dispatch(
        &self,
        tool_name: &str,
        protocol: &str,
        principal: &Principal,
        latency_ms: u64,
        outcome: &Result<Value, TritonError>,
    ) {
        let (result, status) = match outcome {
            Ok(_) => ("ok".to_string(), 200),
            Err(e) => (format!("error:{}", e.class()), status_for(e)),
        };
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Dispatch,
            when: now_rfc3339(),
            who: &principal.sub,
            what: tool_name,
            env: &self.env,
            result,
            protocol,
            tool: tool_name,
            subject: &principal.sub,
            tenant: &principal.tenant,
            latency_ms,
            status,
            trace_id: &principal.trace_id,
        });
    }
}

fn status_for(e: &TritonError) -> u16 {
    match e {
        TritonError::Auth(_) => 401,
        TritonError::Validation(_) => 400,
        TritonError::Tool(_) => 502,
        TritonError::Provider(_) => 502,
    }
}

/// Stable JSON envelope adapters wrap around the dispatch result.
/// PR 5/6/7 fine-tune the per-protocol shaping; PR 4 just needs the
/// round-trip.
pub fn envelope(dispatch: &Dispatch) -> Value {
    json!({
        "result": dispatch.result,
        "trace_id": dispatch.trace_id,
        "latency_ms": dispatch.latency_ms,
    })
}
