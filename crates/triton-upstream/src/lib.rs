//! Upstream router — Consul discovery, Vault per-call OIDC swap,
//! per-tool circuit breaker. Replaces the in-process tool path for
//! tools the registry doesn't carry (FR-U-1..5).
//!
//! Audit symmetry: every successful or failed upstream dispatch
//! emits a `phase: upstream` audit line sharing the inbound's
//! `trace_id` (FR-AU-1).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use triton_core::UpstreamDispatch;
use triton_core::audit::{AuditPhase, AuditRecord, emit, now_rfc3339};
use triton_core::{Principal, TritonError};

pub mod consul;
pub mod vault;

pub use consul::ConsulClient;
pub use vault::VaultClient;

/// Knobs the operator can tune via `TRITON_*` env vars (see
/// `triton-bin::settings`). Numbers chosen to match FR-U-3 defaults.
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub circuit_open_after: u32,
    pub circuit_cooldown: Duration,
    pub upstream_timeout: Duration,
    pub vault_role: String,
    pub env_label: String,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            circuit_open_after: 5,
            circuit_cooldown: Duration::from_secs(30),
            upstream_timeout: Duration::from_secs(10),
            vault_role: "agent-oidc-swap".to_string(),
            env_label: "local".to_string(),
        }
    }
}

pub struct UpstreamRouter {
    consul: ConsulClient,
    vault: VaultClient,
    http: reqwest::Client,
    config: UpstreamConfig,
    breakers: RwLock<HashMap<String, Mutex<Breaker>>>,
}

#[async_trait]
impl UpstreamDispatch for UpstreamRouter {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        UpstreamRouter::invoke(self, tool, args, principal).await
    }
}

impl UpstreamRouter {
    pub fn new(consul: ConsulClient, vault: VaultClient, config: UpstreamConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.upstream_timeout)
            .build()
            .expect("reqwest client");
        Self {
            consul,
            vault,
            http,
            config,
            breakers: RwLock::new(HashMap::new()),
        }
    }

    /// Dispatch `tool` to a Consul-discovered upstream. Emits one
    /// `phase: upstream` audit line per call.
    pub async fn invoke(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        let started = Instant::now();
        let outcome = self.invoke_inner(tool, args, principal).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let (result, status) = match &outcome {
            Ok(_) => ("ok".to_string(), 200),
            Err(e) => (format!("error:{}", e.class()), status_for(e)),
        };
        emit(&AuditRecord {
            kind: "audit",
            phase: AuditPhase::Upstream,
            when: now_rfc3339(),
            who: &principal.sub,
            what: tool,
            env: &self.config.env_label,
            result,
            protocol: "upstream",
            tool,
            subject: &principal.sub,
            tenant: &principal.tenant,
            latency_ms,
            status,
            trace_id: &principal.trace_id,
        });
        outcome
    }

    async fn invoke_inner(
        &self,
        tool: &str,
        args: Value,
        _principal: &Principal,
    ) -> Result<Value, TritonError> {
        // FR-U-4: short-circuit if the per-tool breaker is open.
        // Half-open lets exactly one probe through.
        let permission = self.breaker_check(tool).await;
        if !permission.allowed {
            // Prefix the message with the literal `circuit_open` so
            // `TritonError::is_circuit_open()` can route it to the
            // right HTTP status per architecture §8.3.
            return Err(TritonError::Tool(format!(
                "circuit_open: {tool} (cooldown {}ms)",
                self.config.circuit_cooldown.as_millis()
            )));
        }

        let outcome = self.do_dispatch(tool, args).await;

        // FR-U-3: the breaker only counts tool-side faults (slow or
        // sick agent). Provider faults (Consul/Vault unreachable)
        // have their own retry semantics and MUST NOT push a healthy
        // tool into open. Successes always close.
        let count_failure = matches!(outcome, Err(TritonError::Tool(_)));
        let success = outcome.is_ok();
        if success || count_failure {
            self.breaker_update(tool, permission.was_half_open, success)
                .await;
        }

        outcome
    }

    async fn do_dispatch(&self, tool: &str, args: Value) -> Result<Value, TritonError> {
        // FR-U-1: Consul lookup.
        let endpoint = self
            .consul
            .resolve(tool)
            .await
            .map_err(|e| TritonError::Provider(format!("consul resolve {tool}: {e}")))?
            .ok_or_else(|| TritonError::Provider(format!("no healthy agent for {tool}")))?;

        // FR-U-2 + NFR-S-3: per-call Vault-minted OIDC token,
        // TTL ≤ 5 min. NEVER forward the inbound raw bearer.
        let agent_token = self
            .vault
            .mint_oidc(&self.config.vault_role)
            .await
            .map_err(|e| TritonError::Provider(format!("vault mint: {e}")))?;

        // Per-tool HTTP POST. The path is `/` — agents own their
        // routing inside; Triton just hands them the args body.
        // Error messages omit the resolved URL: clients don't need
        // (and shouldn't get) Triton's internal Consul-resolved
        // routing metadata. Operators read full details from the
        // structured audit/log lines instead.
        let url = format!("http://{endpoint}/");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&agent_token)
            .json(&args)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(tool = %tool, url = %url, error = %e, "upstream send failed");
                if e.is_timeout() {
                    TritonError::Tool(format!("upstream {tool} timed out"))
                } else {
                    TritonError::Tool(format!("upstream {tool} unreachable"))
                }
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            tracing::warn!(tool = %tool, url = %url, %status, "upstream non-2xx");
            return Err(TritonError::Tool(format!(
                "upstream {tool} returned {status}"
            )));
        }
        let body: Value = resp.json().await.map_err(|e| {
            tracing::warn!(tool = %tool, url = %url, error = %e, "upstream decode failed");
            TritonError::Tool(format!("upstream {tool} returned undecodable body"))
        })?;
        Ok(body)
    }

    async fn breaker_check(&self, tool: &str) -> BreakerPermission {
        // Hot path: read-only borrow first, only upgrade to write
        // if we need to install a new breaker.
        if let Some(slot) = self.breakers.read().await.get(tool) {
            return slot
                .lock()
                .unwrap()
                .check_and_arm(self.config.circuit_cooldown);
        }
        // Cold path: install.
        let mut breakers = self.breakers.write().await;
        let slot = breakers
            .entry(tool.to_string())
            .or_insert_with(|| Mutex::new(Breaker::new()));
        slot.get_mut()
            .unwrap()
            .check_and_arm(self.config.circuit_cooldown)
    }

    async fn breaker_update(&self, tool: &str, was_half_open: bool, success: bool) {
        if let Some(slot) = self.breakers.read().await.get(tool) {
            slot.lock()
                .unwrap()
                .observe(success, was_half_open, self.config.circuit_open_after);
        }
    }
}

#[derive(Debug)]
struct Breaker {
    state: BreakerState,
    failures: u32,
    opened_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
struct BreakerPermission {
    allowed: bool,
    /// True when this call is the half-open probe.
    was_half_open: bool,
}

impl Breaker {
    fn new() -> Self {
        Self {
            state: BreakerState::Closed,
            failures: 0,
            opened_at: None,
        }
    }

    fn check_and_arm(&mut self, cooldown: Duration) -> BreakerPermission {
        match self.state {
            BreakerState::Closed => BreakerPermission {
                allowed: true,
                was_half_open: false,
            },
            BreakerState::Open => {
                if self
                    .opened_at
                    .map(|t| t.elapsed() >= cooldown)
                    .unwrap_or(false)
                {
                    self.state = BreakerState::HalfOpen;
                    BreakerPermission {
                        allowed: true,
                        was_half_open: true,
                    }
                } else {
                    BreakerPermission {
                        allowed: false,
                        was_half_open: false,
                    }
                }
            }
            BreakerState::HalfOpen => {
                // Another concurrent probe already in flight — keep
                // failing fast until it settles the breaker.
                BreakerPermission {
                    allowed: false,
                    was_half_open: false,
                }
            }
        }
    }

    fn observe(&mut self, success: bool, was_half_open: bool, open_after: u32) {
        if was_half_open {
            if success {
                self.state = BreakerState::Closed;
                self.failures = 0;
                self.opened_at = None;
            } else {
                self.state = BreakerState::Open;
                self.opened_at = Some(Instant::now());
            }
            return;
        }
        if success {
            self.failures = 0;
            self.state = BreakerState::Closed;
            self.opened_at = None;
        } else {
            self.failures = self.failures.saturating_add(1);
            if self.failures >= open_after {
                self.state = BreakerState::Open;
                self.opened_at = Some(Instant::now());
            }
        }
    }
}

/// Audit-line status mapping. Matches the REST adapter's HTTP
/// mapping in architecture §8.3 so a `phase: upstream` audit line
/// reports the same numeric status a client receives. Diverging
/// (e.g. always logging 502 for any Tool error) would surprise
/// anyone correlating audit logs against client-side response codes.
fn status_for(e: &TritonError) -> u16 {
    if e.is_circuit_open() {
        return 503;
    }
    if e.is_tool_timeout() {
        return 504;
    }
    match e {
        TritonError::Auth(_) => 401,
        TritonError::Validation(_) => 400,
        TritonError::Tool(_) => 502,
        TritonError::Provider(_) => 502,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConsulServiceEntry {
    #[serde(rename = "Service")]
    pub service: ConsulService,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConsulService {
    #[serde(rename = "Address")]
    pub address: String,
    #[serde(rename = "Port")]
    pub port: u16,
    /// FR-U-1: the `agent:<tool>` tag is what makes a service
    /// dispatchable. Untagged services with the same name MUST
    /// be excluded so a stray Consul registration cannot
    /// hijack tool dispatch.
    #[serde(rename = "Tags", default)]
    pub tags: Vec<String>,
}
