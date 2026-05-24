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

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::audit::{AuditPhase, AuditRecord, emit, now_rfc3339};
use crate::error::TritonError;
use crate::metrics::Metrics;
use crate::principal::Principal;
use crate::tool::{ToolDescriptor, ToolRegistry};

/// Hook the dispatcher calls when the in-process registry doesn't
/// know a tool. PR 9's `triton-upstream::UpstreamRouter` implements
/// this; tests can plug in their own implementations. The trait is
/// the dependency-inversion seam between the core dispatcher and
/// the per-substrate router (Consul + Vault + breaker).
#[async_trait]
pub trait UpstreamDispatch: Send + Sync {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError>;
}

#[derive(Debug)]
pub struct Dispatch {
    pub result: Value,
    pub trace_id: String,
    pub latency_ms: u64,
    /// Mirrors the executed tool's `Tool::returns_a2ui()` so the
    /// adapter can decide whether to wrap into an A2UI envelope
    /// without an extra registry lookup (FR-A-5). Upstream-routed
    /// tools default to `false`; PR 10 wraps in-process tools only.
    pub returns_a2ui: bool,
}

pub struct Dispatcher {
    registry: Arc<ToolRegistry>,
    env: String,
    upstream: Option<Arc<dyn UpstreamDispatch>>,
    metrics: Arc<Metrics>,
}

impl Dispatcher {
    pub fn new(registry: Arc<ToolRegistry>, env: impl Into<String>) -> Self {
        Self {
            registry,
            env: env.into(),
            upstream: None,
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Attach a shared `Metrics` registry. When unset, the
    /// dispatcher uses its own private metrics (useful for tests
    /// that don't care about exposition).
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// Attach an upstream-router fallback. Tools not in the registry
    /// are routed through this. Returns `self` for builder-style
    /// composition in `main`.
    pub fn with_upstream(mut self, upstream: Arc<dyn UpstreamDispatch>) -> Self {
        self.upstream = Some(upstream);
        self
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// Public descriptors for the `GET /v1/tools` listing (FR-A-5).
    /// Adapters never reach into the registry directly — they go
    /// through this method so the dispatcher stays the single seam
    /// between adapters and tool-state.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.registry.descriptors()
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
        let returns_a2ui = self
            .registry
            .get(tool_name)
            .map(|t| t.returns_a2ui())
            .unwrap_or(false);
        outcome.map(|result| Dispatch {
            result,
            trace_id: principal.trace_id,
            latency_ms,
            returns_a2ui,
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
        let result = format!("error:{}", error.class());
        self.metrics.record_dispatch(tool_name, protocol, &result);
        self.metrics.record_audit("rejected");
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Rejected,
            when: now_rfc3339(),
            who: subject,
            what: tool_name,
            env: &self.env,
            result,
            protocol,
            tool: tool_name,
            subject,
            tenant,
            latency_ms: 0,
            status,
            status_label: None,
            trace_id,
        });
    }

    /// Emit a `phase: post` audit line for the chat-channel
    /// outbound courier (PR 18). The adapter has already attempted
    /// to ship the tool result back to the platform (Telegram,
    /// Discord, ...); call this with `Ok(http_status)` on success
    /// or `Err(&TritonError)` on failure. Schema construction stays
    /// in the dispatcher so the courier crate doesn't grow its own
    /// audit emitter (ADR-6 single pivot).
    ///
    /// `tool_name` is whatever tool the original inbound triggered;
    /// `latency_ms` covers ONLY the post-back HTTP roundtrip, not
    /// the inbound dispatch (that's the previous `phase: dispatch`
    /// line).
    pub fn record_post(
        &self,
        tool_name: &str,
        protocol: &str,
        principal: &Principal,
        latency_ms: u64,
        outcome: Result<(u16, &'static str), (&TritonError, u16, &'static str)>,
    ) {
        // FR-AU-1 v0.2: chat post audit MUST carry a `status_label`
        // from the closed set `{posted, retry, dropped}`. We keep
        // `status` as the underlying HTTP status (`u16`, 0 for
        // transport-level failures) for diagnosis, and add a new
        // `status_label` field for the spec's closed-set discriminator.
        let (result, status, status_label) = match outcome {
            Ok((s, label)) => ("ok".to_string(), s, Some(label)),
            Err((e, s, label)) => (format!("error:{}", e.class()), s, Some(label)),
        };
        self.metrics.record_dispatch(tool_name, protocol, &result);
        self.metrics.record_audit("post");
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Post,
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
            status_label,
            trace_id: &principal.trace_id,
        });
    }

    async fn run(
        &self,
        tool_name: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        // In-process tool wins. Falls back to the upstream router
        // for tools not present in the registry (PR 9). When no
        // upstream is configured AND the tool is unknown, surface
        // Validation so the caller learns the dispatcher couldn't
        // route the call.
        if let Some(tool) = self.registry.get(tool_name) {
            // Spawn so a panic inside the tool becomes a JoinError
            // we can translate to TritonError::Tool — keeps the
            // "one audit line per invocation" guarantee.
            let tp = principal.to_tool_principal();
            let join = tokio::spawn(async move { tool.invoke(args, &tp).await });
            return match join.await {
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
            };
        }
        if let Some(upstream) = &self.upstream {
            return upstream.invoke(tool_name, args, principal).await;
        }
        Err(TritonError::Validation(format!(
            "unknown tool: {tool_name}"
        )))
    }

    fn fail(
        &self,
        tool_name: &str,
        protocol: &str,
        principal: &Principal,
        error: &TritonError,
        latency_ms: u64,
    ) -> TritonError {
        let result = format!("error:{}", error.class());
        self.metrics.record_dispatch(tool_name, protocol, &result);
        self.metrics.record_audit("dispatch");
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
            status: status_for(error),
            status_label: None,
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
        self.metrics.record_dispatch(tool_name, protocol, &result);
        self.metrics.record_audit("dispatch");
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
            status_label: None,
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
