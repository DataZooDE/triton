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
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};

use crate::audit::{AuditPhase, AuditRecord, PostOutcome, emit, now_rfc3339};
use crate::error::TritonError;
use crate::metrics::Metrics;
use crate::principal::Principal;
use crate::stream::{Finalized, StreamEvent, Termination, Timing};
use crate::tool::{ToolDescriptor, ToolRegistry};

/// Hook the dispatcher calls when the in-process registry doesn't
/// know a tool. `triton-upstream::StaticUpstream` implements this;
/// tests can plug in their own implementations. The trait is the
/// dependency-inversion seam between the core dispatcher and the
/// upstream dispatcher (a static `host:port` map + per-call RS256 JWT).
/// Outcome handed to [`Dispatcher::record_post`]. `Ok` carries
/// `(http_status, disposition, optional detail)`; `Err` carries the
/// error plus the same triple. The `detail` is a free-form
/// diagnostic reason (e.g. `modal_opened`, `rasterizer_call`) that
/// rides alongside the closed-set [`PostOutcome`].
pub type PostResult<'a> = Result<
    (u16, PostOutcome, Option<&'static str>),
    (&'a TritonError, u16, PostOutcome, Option<&'static str>),
>;

#[async_trait]
pub trait UpstreamDispatch: Send + Sync {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError>;

    /// Streaming counterpart of [`Self::invoke`] (issue #132).
    ///
    /// `Ok(stream)` means the upstream accepted the call and a response
    /// stream is open (200 headers flushed) — the stream yields typed
    /// [`StreamEvent`]s terminated by exactly one `Done`/`Error`.
    /// `Err(e)` is a *pre-first-byte* failure (unknown tool, open
    /// breaker, connect error, non-2xx) the caller can still surface as
    /// a normal HTTP error before committing to an SSE response.
    ///
    /// The default adapts the buffered [`Self::invoke`] into a single
    /// terminal `Done` event (or a pre-first-byte `Err`), so the
    /// in-process registry and non-streaming upstreams need no override.
    async fn invoke_streaming(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
        let value = self.invoke(tool, args, principal).await?;
        Ok(stream::once(async move { StreamEvent::Done(value) }).boxed())
    }

    /// Tool names of upstream agents discoverable right now (the keys
    /// of the `TRITON_STATIC_UPSTREAMS` map). Surfaced by `GET /v1/tools`
    /// so clients can discover agents that aren't in the in-process
    /// registry. The default returns nothing; the real dispatcher
    /// degrades to empty on error (listing must never fail the
    /// endpoint).
    async fn list_agents(&self) -> Vec<String> {
        Vec::new()
    }
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

    /// Like [`Self::descriptors`], but also folds in upstream agents
    /// from the static upstream map (`TRITON_STATIC_UPSTREAMS`),
    /// flagged `upstream: true`. In-process tools win on a name clash.
    /// Upstream discovery degrades to "just the in-process tools" if the
    /// router is unavailable — listing never fails. Result is
    /// name-sorted for a stable `GET /v1/tools` order.
    pub async fn descriptors_all(&self) -> Vec<ToolDescriptor> {
        let mut out = self.registry.descriptors();
        if let Some(upstream) = &self.upstream {
            let known: std::collections::HashSet<String> =
                out.iter().map(|d| d.name.clone()).collect();
            for name in upstream.list_agents().await {
                if !known.contains(&name) {
                    out.push(ToolDescriptor {
                        name,
                        input_schema: json!({}),
                        // Triton can't know an agent's schema; assume it
                        // may emit a surface so the UI offers A2UI.
                        returns_a2ui: true,
                        upstream: true,
                    });
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
        }
        out
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

    /// Streaming counterpart of [`Self::invoke_with_bytes`] for the SSE
    /// response path (issue #132).
    ///
    /// `Ok(stream)` is an open response: the stream yields typed
    /// [`StreamEvent`]s terminated by exactly one `Done`/`Error`, and the
    /// single ADR-6 dispatch audit line is emitted at **stream
    /// termination** (clean close, mid-stream error, or client
    /// disconnect) via a [`Finalized`] combinator — so the audit
    /// guarantee survives not knowing the outcome until close.
    ///
    /// `Err(e)` is a *pre-first-byte* failure (bad JSON, unknown tool,
    /// open breaker, upstream connect error / non-2xx). It is audited
    /// inline here, exactly once, and the adapter surfaces it as an
    /// ordinary HTTP error response (no SSE headers flushed yet).
    ///
    /// In-process tools and the no-upstream case don't stream: they run
    /// buffered, audit immediately (latency == total), and wrap the lone
    /// result into a one-item stream.
    ///
    /// `a2ui` is the caller's negotiated A2UI version (if any). When set,
    /// the terminal `Done` payload is wrapped into the versioned envelope
    /// — **before** the audit finalizer, so a tool that advertised A2UI
    /// but emitted a non-surface turns the terminal into an `Error` that
    /// the single audit line reflects (the finalizer stays outermost).
    pub async fn invoke_streaming_with_bytes(
        &self,
        tool_name: &str,
        body: &[u8],
        principal: Principal,
        protocol: &str,
        a2ui: Option<crate::A2uiVersion>,
    ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
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
        self.invoke_streaming(tool_name, args, principal, protocol, a2ui)
            .await
    }

    /// Streaming dispatch on pre-parsed args (the A2A path), the
    /// counterpart of [`Self::invoke`]. See
    /// [`Self::invoke_streaming_with_bytes`] for the contract.
    pub async fn invoke_streaming(
        &self,
        tool_name: &str,
        args: Value,
        principal: Principal,
        protocol: &str,
        a2ui: Option<crate::A2uiVersion>,
    ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
        let started = Instant::now();

        // In-process tools (and the no-upstream fallback) are not
        // streaming sources: run them buffered, audit now, and emit a
        // single terminal `Done`. A pre-first-byte error audits inline.
        if let Some(tool) = self.registry.get(tool_name) {
            let returns_a2ui = tool.returns_a2ui();
            let outcome = self.run(tool_name, args, &principal).await;
            let latency_ms = started.elapsed().as_millis() as u64;
            self.audit_dispatch(tool_name, protocol, &principal, latency_ms, &outcome);
            let value = outcome?;
            // Mirror the buffered path: only wrap when the tool opted in
            // AND the caller negotiated A2UI.
            let done = match (a2ui, returns_a2ui) {
                (Some(version), true) => done_event_a2ui(value, version),
                _ => StreamEvent::Done(value),
            };
            return Ok(stream::once(async move { done }).boxed());
        }
        if self.upstream.is_none() {
            let outcome = self.run(tool_name, args, &principal).await;
            let latency_ms = started.elapsed().as_millis() as u64;
            self.audit_dispatch(tool_name, protocol, &principal, latency_ms, &outcome);
            let value = outcome?;
            return Ok(stream::once(async move { StreamEvent::Done(value) }).boxed());
        }

        // Upstream path. A 200 stream defers its single audit line to
        // termination; a pre-first-byte error audits inline and returns.
        let upstream = self.upstream.as_ref().expect("upstream present");
        match upstream.invoke_streaming(tool_name, args, &principal).await {
            Err(e) => Err(self.fail(
                tool_name,
                protocol,
                &principal,
                &e,
                started.elapsed().as_millis() as u64,
            )),
            Ok(inner) => {
                // A2UI-wrap the terminal `Done` *inside* the finalizer so
                // the audit observes the post-transform terminal (an agent
                // advertised A2UI but emitted a non-surface → Error).
                let inner = match a2ui {
                    Some(version) => wrap_stream_a2ui(inner, version),
                    None => inner,
                };
                // Everything the single audit line needs, captured by the
                // finalizer that fires once at stream termination.
                let metrics = self.metrics.clone();
                let env = self.env.clone();
                let protocol = protocol.to_string();
                let tool = tool_name.to_string();
                let sub = principal.sub.clone();
                let tenant = principal.tenant.clone();
                let trace_id = principal.trace_id.clone();
                // Offset from request start to the moment the stream
                // opened, so `ttfb`/`total` reflect the whole request, not
                // just the post-200 window the combinator clocks.
                let open_offset = started.elapsed();
                let finalized = Finalized::new(inner, move |term: Termination, timing: Timing| {
                    let total_ms = (open_offset + timing.total).as_millis() as u64;
                    let ttfb_ms = timing.ttfb.map(|t| (open_offset + t).as_millis() as u64);
                    emit_stream_audit(StreamAudit {
                        metrics: &metrics,
                        env: &env,
                        protocol: &protocol,
                        tool: &tool,
                        sub: &sub,
                        tenant: &tenant,
                        trace_id: &trace_id,
                        term,
                        total_ms,
                        ttfb_ms,
                    });
                });
                Ok(finalized.boxed())
            }
        }
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
            // Upstream agents aren't in the in-process registry. A successful
            // dispatch with no in-process tool of this name went upstream, and
            // agents are assumed to emit a surface (mirrors `descriptors_all`),
            // so A2UI version mapping (surface → stream) applies to them too.
            .unwrap_or(self.upstream.is_some());
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
            status_detail: None,
            ttfb_ms: None,
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
        outcome: PostResult<'_>,
    ) {
        // FR-AU-1 v0.2: chat post audit MUST carry a `status_label`
        // from the closed set `{posted, retry, dropped}` — enforced by
        // the `PostOutcome` type. We keep `status` as the underlying
        // HTTP status (`u16`, 0 for transport-level failures) for
        // diagnosis. Any finer reason rides on `status_detail`.
        let (result, status, status_label, detail) = match outcome {
            Ok((s, label, detail)) => ("ok".to_string(), s, Some(label.as_str()), detail),
            Err((e, s, label, detail)) => (
                format!("error:{}", e.class()),
                s,
                Some(label.as_str()),
                detail,
            ),
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
            status_detail: detail,
            ttfb_ms: None,
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
            status_detail: None,
            ttfb_ms: None,
            trace_id: &principal.trace_id,
        });
        // Reconstruct a parallel error so we can both audit and return.
        match error {
            TritonError::Auth(m) => TritonError::Auth(m.clone()),
            TritonError::Forbidden(m) => TritonError::Forbidden(m.clone()),
            TritonError::Validation(m) => TritonError::Validation(m.clone()),
            TritonError::Tool(m) => TritonError::Tool(m.clone()),
            TritonError::Provider(m) => TritonError::Provider(m.clone()),
            TritonError::RateLimited(m) => TritonError::RateLimited(m.clone()),
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
            status_detail: None,
            ttfb_ms: None,
            trace_id: &principal.trace_id,
        });
    }
}

fn status_for(e: &TritonError) -> u16 {
    // Single source of truth in TritonError so the audit status
    // matches what REST/A2A return (circuit_open → 503, timeout → 504).
    e.http_status()
}

/// Wrap a single result value into a terminal A2UI [`StreamEvent`],
/// reusing the same `extract_surface`/`build_envelope` path as the
/// buffered adapter. A non-surface payload becomes a terminal `Error`
/// (mirrors the buffered path's `TritonError::Tool`), surfaced to the
/// client as an `error` frame.
fn done_event_a2ui(value: Value, version: crate::A2uiVersion) -> StreamEvent {
    match crate::a2ui::extract_surface(&value) {
        Ok(surface) => StreamEvent::Done(crate::a2ui::build_envelope(&surface, version.into())),
        Err(e) => StreamEvent::Error(TritonError::Tool(format!("tool advertised A2UI but {e}"))),
    }
}

/// Map a stream's terminal `Done` through [`done_event_a2ui`], passing
/// `tool`/`token`/`error` frames through untouched. Used inside the
/// audit finalizer so the wrapped terminal is what gets audited.
fn wrap_stream_a2ui(
    inner: BoxStream<'static, StreamEvent>,
    version: crate::A2uiVersion,
) -> BoxStream<'static, StreamEvent> {
    inner
        .map(move |ev| match ev {
            StreamEvent::Done(v) => done_event_a2ui(v, version),
            other => other,
        })
        .boxed()
}

/// Arguments for [`emit_stream_audit`]. Grouped into one struct so the
/// terminal-state audit emission stays a single call with named fields
/// rather than a long positional argument list.
struct StreamAudit<'a> {
    metrics: &'a Metrics,
    env: &'a str,
    protocol: &'a str,
    tool: &'a str,
    sub: &'a str,
    tenant: &'a str,
    trace_id: &'a str,
    term: Termination,
    total_ms: u64,
    ttfb_ms: Option<u64>,
}

/// Emit the single ADR-6 dispatch audit line for a *streamed* invocation
/// at its terminal state (issue #132). Called exactly once by the
/// [`Finalized`] combinator's finalizer.
///
/// The outcome maps to the audit `result`/`status` like so:
/// clean `Done` → `ok`/200; mid-stream `Error` → `error:tool`/502;
/// upstream truncation → `error:tool`/502 (`status_detail:
/// upstream_truncated`); client disconnect → `client_disconnect`/499.
fn emit_stream_audit(a: StreamAudit<'_>) {
    let (result, status, status_detail): (String, u16, Option<&'static str>) = match a.term {
        Termination::Completed => ("ok".to_string(), 200, None),
        Termination::Failed => ("error:tool".to_string(), 502, None),
        Termination::Truncated => ("error:tool".to_string(), 502, Some("upstream_truncated")),
        Termination::Disconnected => (
            "client_disconnect".to_string(),
            499,
            Some("client_disconnect"),
        ),
    };
    a.metrics.record_dispatch(a.tool, a.protocol, &result);
    a.metrics.record_audit("dispatch");
    emit(&AuditRecord {
        kind: "audit",
        phase: AuditPhase::Dispatch,
        when: now_rfc3339(),
        who: a.sub,
        what: a.tool,
        env: a.env,
        result,
        protocol: a.protocol,
        tool: a.tool,
        subject: a.sub,
        tenant: a.tenant,
        latency_ms: a.total_ms,
        status,
        status_label: None,
        status_detail,
        ttfb_ms: a.ttfb_ms,
        trace_id: a.trace_id,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An upstream that returns a surface, like a real agent.
    struct SurfaceUpstream;

    #[async_trait]
    impl UpstreamDispatch for SurfaceUpstream {
        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _principal: &Principal,
        ) -> Result<Value, TritonError> {
            Ok(json!({ "surface": { "components": [] } }))
        }
        async fn list_agents(&self) -> Vec<String> {
            vec!["assistant".to_string()]
        }
    }

    fn test_principal() -> Principal {
        Principal {
            sub: "tester".into(),
            scopes: Vec::new(),
            groups: Vec::new(),
            tenant: "default".into(),
            raw_token: String::new(),
            trace_id: "trace-test".into(),
        }
    }

    /// An upstream agent isn't in the in-process registry, but its surface
    /// must still be A2UI-mapped (surface → versioned stream) — so a
    /// successful upstream dispatch reports `returns_a2ui = true`, mirroring
    /// `descriptors_all`. Regression for the blank rendered-surface bug:
    /// before the fix the flag defaulted to `false` and the renderer got the
    /// raw surface instead of a stream.
    #[tokio::test]
    async fn upstream_dispatch_reports_returns_a2ui() {
        let dispatcher = Dispatcher::new(Arc::new(ToolRegistry::new()), "test")
            .with_upstream(Arc::new(SurfaceUpstream));
        let dispatch = dispatcher
            .invoke("assistant", json!({}), test_principal(), "rest")
            .await
            .expect("upstream dispatch succeeds");
        assert!(
            dispatch.returns_a2ui,
            "upstream agent dispatch must report returns_a2ui=true so Triton maps its surface to an A2UI stream",
        );
    }

    /// Guard the seam: with no upstream configured, a successful in-process
    /// dispatch is unaffected and a name miss still errors (never fabricates
    /// `returns_a2ui`). Here the registry is empty and there's no upstream, so
    /// the unknown tool is a genuine error, not a silent a2ui=true.
    #[tokio::test]
    async fn missing_tool_without_upstream_errors() {
        let dispatcher = Dispatcher::new(Arc::new(ToolRegistry::new()), "test");
        let result = dispatcher
            .invoke("nope", json!({}), test_principal(), "rest")
            .await;
        assert!(result.is_err(), "unknown tool with no upstream must error");
    }
}
