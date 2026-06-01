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
pub mod static_upstream;
pub mod vault;

pub use consul::ConsulClient;
pub use static_upstream::StaticUpstream;
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

    /// List discoverable `agent:<name>` services for `GET /v1/tools`.
    /// Degrades to empty on any Consul error — discovery is best-effort
    /// and must never fail the listing endpoint.
    async fn list_agents(&self) -> Vec<String> {
        match self.consul.list_agent_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                tracing::warn!(error = %e, "consul agent listing failed; omitting upstream tools");
                Vec::new()
            }
        }
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
            status_label: None,
            status_detail: None,
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

        // SSRF guard (NFR-S-4): a poisoned Consul catalog entry could
        // point us at an arbitrary host while we carry a freshly
        // minted agent bearer. Substrate agents live on the tailnet
        // (private / 100.64-CGNAT) or are named via Consul DNS; refuse
        // public and link-local IP targets (e.g. 169.254.169.254 cloud
        // metadata) *before* minting a token or dialing.
        if !endpoint_is_dispatchable(&endpoint) {
            tracing::warn!(tool = %tool, "upstream endpoint rejected by SSRF guard");
            return Err(TritonError::Provider(format!(
                "upstream endpoint for {tool} is not a permitted tailnet/private target"
            )));
        }

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
            .header("X-Triton-Tool", tool)
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
        TritonError::RateLimited(_) => 429,
    }
}

/// SSRF guard for a Consul-resolved `host:port` endpoint. IP literals
/// must be loopback, RFC-1918 private, or CGNAT/Tailscale
/// (100.64.0.0/10 v4, `fc00::/7` ULA v6). Hostnames are trusted ONLY
/// under Consul's own `.consul` domain (`*.service.consul`) — an
/// arbitrary hostname could resolve to a public or metadata IP, and
/// non-canonical numeric forms (octal/hex/decimal) that `IpAddr`
/// won't parse must not slip through the hostname path either. Public
/// and link-local targets — notably `169.254.169.254` cloud metadata
/// — are refused. (Codex security review.)
fn endpoint_is_dispatchable(endpoint: &str) -> bool {
    // Split off the port; tolerate a bracketed IPv6 host.
    let host = endpoint
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(endpoint);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        // Not an IP literal → only trust Consul DNS names. This also
        // rejects non-canonical IP encodings (e.g. `0177.0.0.1`,
        // `2130706433`) that `IpAddr` refuses to parse.
        Err(_) => {
            let h = host.trim_end_matches('.').to_ascii_lowercase();
            h.ends_with(".consul")
        }
        Ok(std::net::IpAddr::V4(v4)) => {
            if v4.is_loopback() || v4.is_private() {
                return true;
            }
            // CGNAT 100.64.0.0/10 — Tailscale's tailnet range.
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        // Loopback (::1) or unique-local (fc00::/7, which includes
        // Tailscale's fd7a:… range). Global + link-local are refused.
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback() || (v6.octets()[0] & 0xfe) == 0xfc,
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::endpoint_is_dispatchable;

    #[test]
    fn allows_loopback_private_tailnet_and_hostnames() {
        assert!(endpoint_is_dispatchable("127.0.0.1:8080"));
        assert!(endpoint_is_dispatchable("10.1.2.3:443"));
        assert!(endpoint_is_dispatchable("192.168.0.5:80"));
        assert!(endpoint_is_dispatchable("172.16.9.9:80"));
        assert!(endpoint_is_dispatchable("100.96.1.2:8001")); // tailnet
        assert!(endpoint_is_dispatchable("demo-agent.service.consul:8001"));
        assert!(endpoint_is_dispatchable("[::1]:8080"));
        assert!(endpoint_is_dispatchable("[fd7a:115c:a1e0::1]:8080")); // ULA
    }

    #[test]
    fn refuses_public_and_metadata_targets() {
        assert!(!endpoint_is_dispatchable("169.254.169.254:80")); // cloud metadata
        assert!(!endpoint_is_dispatchable("1.2.3.4:80")); // public
        assert!(!endpoint_is_dispatchable("8.8.8.8:53")); // public
        assert!(!endpoint_is_dispatchable("[2606:4700:4700::1111]:443")); // public v6
    }

    #[test]
    fn refuses_arbitrary_hostnames_and_noncanonical_ip_encodings() {
        // Codex bypass cases: an arbitrary hostname could resolve to
        // a public/metadata IP, and non-canonical numeric encodings
        // don't parse as an IP — neither may take the hostname path.
        assert!(!endpoint_is_dispatchable("evil.example:80"));
        assert!(!endpoint_is_dispatchable("metadata.google.internal:80"));
        assert!(!endpoint_is_dispatchable("0177.0.0.1:80")); // octal 127.0.0.1
        assert!(!endpoint_is_dispatchable("2130706433:80")); // decimal 127.0.0.1
        assert!(!endpoint_is_dispatchable("0x7f.0.0.1:80")); // hex
        // A `.consul`-suffixed lookalike under an attacker domain is
        // still not a `.consul` name.
        assert!(!endpoint_is_dispatchable("consul.evil.com:80"));
        // Trailing-dot + mixed case Consul name is still accepted.
        assert!(endpoint_is_dispatchable("Demo-Agent.Service.Consul.:8001"));
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
