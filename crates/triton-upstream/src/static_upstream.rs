//! Static upstream dispatch (issue #75): resolve a tool to a fixed
//! `host:port` from a static map and POST the args there — **no Consul**.
//!
//! Two auth modes for the upstream bearer:
//!   * **static token** (default `dev-token`) — local "standalone sidecar" dev
//!     against an agent built with the `dev-token` affordance.
//!   * **signed JWT** — when a [`JwtSigner`] is attached (`with_signer`), Triton
//!     mints a short-lived RS256 OIDC token per call instead, so PRODUCTION
//!     agents (dev-token compiled out, ADR-10) verify it through their normal
//!     `AGENT_OIDC_ISSUER` path — workload→workload auth without Vault. This is
//!     the Consul-less, Vault-less dispatch path (and the only one).
//!
//! Two protections survive the Consul/Vault decommission, ported here from the
//! old `UpstreamRouter`:
//!   * an **SSRF guard** ([`endpoint_is_dispatchable`]) on every mapped
//!     endpoint — enforced at boot outside `local` env so a mis-templated or
//!     compromised `TRITON_STATIC_UPSTREAMS` can't point Triton (carrying a
//!     freshly-minted agent bearer) at a public/metadata host;
//!   * a **per-tool circuit breaker** (FR-U-3/4) so a sick agent fails fast
//!     instead of making every caller wait out the per-call timeout.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;
use triton_core::{Principal, TritonError, UpstreamDispatch};
use triton_identity::JwtSigner;

/// Upstream OIDC token TTL (NFR-S-3 cap is enforced by the signer too).
const TOKEN_TTL: Duration = Duration::from_secs(300);

/// #114 caps on resolver-supplied principal data forwarded into the
/// minted token, so a buggy/hostile resolver can't bloat or corrupt it.
const MAX_SCOPES: usize = 32;
const MAX_SCOPE_LEN: usize = 64;
const MAX_TENANT_LEN: usize = 128;

/// Sanitise resolver-supplied tokens (scopes or groups) before they're
/// signed into the `triton_sender_scopes` / `triton_sender_groups` claim
/// (#114 / RBAC): drop empty / whitespace-bearing / over-length values,
/// apply the operator allowlist when configured, and cap the count. Pure so
/// it's unit-testable.
fn sanitise_scopes(scopes: &[String], allowlist: Option<&HashSet<String>>) -> Vec<String> {
    scopes
        .iter()
        .filter(|s| !s.is_empty() && s.len() <= MAX_SCOPE_LEN && !s.contains(char::is_whitespace))
        .filter(|s| allowlist.is_none_or(|a| a.contains(s.as_str())))
        .take(MAX_SCOPES)
        .cloned()
        .collect()
}

pub struct StaticUpstream {
    map: HashMap<String, String>,
    token: String,
    http: reqwest::Client,
    /// When set, each call's bearer is a freshly-signed RS256 JWT (aud =
    /// `audience`, sub = the caller principal) instead of the static `token`.
    signer: Option<Arc<JwtSigner>>,
    /// `aud` claim for minted JWTs. May be a comma-separated list to name
    /// several intended recipients in one token (e.g.
    /// `agents-nonprod,escurel-nonprod` — the agent verifies `agents-nonprod`
    /// and forwards the same token to escurel, which verifies `escurel-nonprod`).
    /// Ignored when `signer` is None.
    audience: String,
    /// `tenant` claim for minted JWTs (a forwarded-to downstream like Escurel
    /// may key its tenant off it). Empty → no `tenant` claim. Ignored when
    /// `signer` is None.
    tenant: String,
    /// #110: when true, the minted token carries the RESOLVED SENDER's
    /// identity — `tenant` ← `principal.tenant` and a space-delimited
    /// `scope` ← `principal.scopes` — instead of the deployment-static
    /// `tenant` and no scopes. Opt-in (default false) so the default
    /// contract is unchanged. Ignored when `signer` is None.
    forward_principal: bool,
    /// #114: optional operator allowlist of scopes that may be forwarded
    /// (the `triton_sender_scopes` claim). `Some` → forwarded scopes are
    /// `principal.scopes ∩ allowlist`; `None` → caps only. Ignored unless
    /// `forward_principal`.
    forward_scope_allowlist: Option<HashSet<String>>,
    /// RBAC: optional operator allowlist of groups that may be forwarded
    /// (the `triton_sender_groups` claim). `Some` → forwarded groups are
    /// `principal.groups ∩ allowlist`; `None` → caps only. Ignored unless
    /// `forward_principal`.
    forward_group_allowlist: Option<HashSet<String>>,
    /// FR-U-3/4 per-tool circuit breaker, keyed by tool name.
    breakers: RwLock<HashMap<String, Mutex<Breaker>>>,
    /// Consecutive tool-side faults that trip a breaker open.
    circuit_open_after: u32,
    /// How long a tripped breaker stays open before a half-open probe.
    circuit_cooldown: Duration,
}

impl StaticUpstream {
    /// Parse `name=host:port,name2=host:port` into the static map. The
    /// `token` is sent as the upstream bearer (default `dev-token`, which
    /// a dev-token agent accepts) unless a signer is attached.
    pub fn from_spec(
        spec: &str,
        token: String,
        timeout: Duration,
        circuit_open_after: u32,
        circuit_cooldown: Duration,
    ) -> Self {
        let map = spec
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self {
            map,
            token,
            http,
            signer: None,
            audience: String::new(),
            tenant: String::new(),
            forward_principal: false,
            forward_scope_allowlist: None,
            forward_group_allowlist: None,
            breakers: RwLock::new(HashMap::new()),
            circuit_open_after,
            circuit_cooldown,
        }
    }

    /// Attach a JWT signer: every dispatch now carries a freshly-minted RS256
    /// token with `aud = audience` and (when non-empty) `tenant`, instead of
    /// the static bearer. Pair with serving the signer's JWKS so agents can
    /// verify (see `triton-bin`).
    pub fn with_signer(
        mut self,
        signer: Arc<JwtSigner>,
        audience: impl Into<String>,
        tenant: impl Into<String>,
        forward_principal: bool,
        forward_scope_allowlist: Option<HashSet<String>>,
        forward_group_allowlist: Option<HashSet<String>>,
    ) -> Self {
        self.signer = Some(signer);
        self.audience = audience.into();
        self.tenant = tenant.into();
        self.forward_principal = forward_principal;
        self.forward_scope_allowlist = forward_scope_allowlist;
        self.forward_group_allowlist = forward_group_allowlist;
        self
    }

    /// Mapped `(tool, endpoint)` pairs whose endpoint fails the SSRF guard
    /// ([`endpoint_is_dispatchable`]). `triton-bin` calls this at boot and
    /// refuses to start (outside `local` env) if any are returned, so a
    /// mis-templated `TRITON_STATIC_UPSTREAMS` fails closed rather than
    /// dialling an attacker/metadata host with a minted bearer.
    ///
    /// `allowed_suffixes` is the operator-configured set of trusted DNS
    /// suffixes a hostname endpoint may end with (e.g. `[".ts.net"]` by
    /// default, optionally widened to a private split-DNS domain like
    /// `.int.data-zoo.de`). IP-literal rules are independent of it.
    pub fn undispatchable_endpoints(&self, allowed_suffixes: &[String]) -> Vec<(String, String)> {
        let mut bad: Vec<(String, String)> = self
            .map
            .iter()
            .filter(|(_, ep)| !endpoint_is_dispatchable(ep, allowed_suffixes))
            .map(|(t, ep)| (t.clone(), ep.clone()))
            .collect();
        bad.sort();
        bad
    }

    /// The per-call bearer: a fresh signed JWT when a signer is attached, else
    /// the static token.
    fn bearer(&self, principal: &Principal) -> Result<String, TritonError> {
        match &self.signer {
            Some(s) => {
                // Comma-separated audiences → a multi-aud token (each hop pins
                // its own). Trimmed; empties dropped.
                let auds: Vec<&str> = self
                    .audience
                    .split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .collect();
                // #110: opt-in, forward the resolved sender's tenant + scopes;
                // otherwise the deployment-static tenant and no scopes.
                // #114: resolver-supplied values are sanitised/capped (and
                // allowlisted) before signing — see `sanitise_scopes`.
                let (tenant, scopes, groups): (String, Vec<String>, Vec<String>) = if self
                    .forward_principal
                {
                    let tenant = if principal.tenant.len() <= MAX_TENANT_LEN {
                        principal.tenant.clone()
                    } else {
                        tracing::warn!(
                            len = principal.tenant.len(),
                            "forwarded tenant over cap; dropping"
                        );
                        String::new()
                    };
                    let scopes =
                        sanitise_scopes(&principal.scopes, self.forward_scope_allowlist.as_ref());
                    // RBAC: same sanitise/cap/allowlist as scopes. Rides
                    // `triton_sender_groups`, never `roles`.
                    let groups =
                        sanitise_scopes(&principal.groups, self.forward_group_allowlist.as_ref());
                    (tenant, scopes, groups)
                } else {
                    (self.tenant.clone(), Vec::new(), Vec::new())
                };
                s.sign(&auds, &principal.sub, &tenant, &scopes, &groups, TOKEN_TTL)
                    .map_err(|e| TritonError::Tool(format!("mint upstream token: {e}")))
            }
            None => Ok(self.token.clone()),
        }
    }

    /// Dispatch to the mapped endpoint. `TritonError::Tool` for agent-side
    /// faults (unreachable / non-2xx / undecodable) so the breaker counts
    /// them; `Validation` for an unknown tool (a caller fault, not an
    /// agent fault — never trips the breaker).
    async fn do_dispatch(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        let ep = self
            .map
            .get(tool)
            .ok_or_else(|| TritonError::Validation(format!("unknown tool: {tool}")))?;
        let bearer = self.bearer(principal)?;
        let resp = self
            .http
            .post(format!("http://{ep}/"))
            .bearer_auth(&bearer)
            // Contract parity with the Consul-mode router (#101): the
            // informational tool-name header rides every dispatch so
            // multi-tool agents can route without sniffing the body.
            .header("X-Triton-Tool", tool)
            .json(&args)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    TritonError::Tool(format!("upstream {tool} timed out"))
                } else {
                    TritonError::Tool(format!("upstream {tool} unreachable: {e}"))
                }
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(TritonError::Tool(format!(
                "upstream {tool} returned {status}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| TritonError::Tool(format!("upstream {tool} decode: {e}")))
    }

    async fn breaker_check(&self, tool: &str) -> BreakerPermission {
        // Hot path: read-only borrow first, only upgrade to write
        // if we need to install a new breaker.
        if let Some(slot) = self.breakers.read().await.get(tool) {
            return slot.lock().unwrap().check_and_arm(self.circuit_cooldown);
        }
        let mut breakers = self.breakers.write().await;
        let slot = breakers
            .entry(tool.to_string())
            .or_insert_with(|| Mutex::new(Breaker::new()));
        slot.get_mut().unwrap().check_and_arm(self.circuit_cooldown)
    }

    async fn breaker_update(&self, tool: &str, was_half_open: bool, success: bool) {
        if let Some(slot) = self.breakers.read().await.get(tool) {
            slot.lock()
                .unwrap()
                .observe(success, was_half_open, self.circuit_open_after);
        }
    }
}

#[async_trait]
impl UpstreamDispatch for StaticUpstream {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        principal: &Principal,
    ) -> Result<Value, TritonError> {
        // FR-U-4: short-circuit if the per-tool breaker is open. Half-open
        // lets exactly one probe through. The `circuit_open` prefix routes
        // the error to a 503 via `TritonError::is_circuit_open()`.
        let permission = self.breaker_check(tool).await;
        if !permission.allowed {
            return Err(TritonError::Tool(format!(
                "circuit_open: {tool} (cooldown {}ms)",
                self.circuit_cooldown.as_millis()
            )));
        }

        let outcome = self.do_dispatch(tool, args, principal).await;

        // FR-U-3: only agent-side faults (Tool) count toward the breaker;
        // an unknown tool (Validation) is a caller fault and must not trip
        // a healthy agent. Successes always close.
        let count_failure = matches!(outcome, Err(TritonError::Tool(_)));
        let success = outcome.is_ok();
        if success || count_failure {
            self.breaker_update(tool, permission.was_half_open, success)
                .await;
        }
        outcome
    }

    async fn list_agents(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
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

/// SSRF guard for a `TRITON_STATIC_UPSTREAMS` `host:port` endpoint. IP
/// literals must be loopback, RFC-1918 private, or CGNAT/Tailscale
/// (100.64.0.0/10 v4, `fc00::/7` ULA v6). Hostnames are trusted ONLY when
/// they end with one of `allowed_suffixes` — by default exactly `.ts.net`
/// (Tailscale MagicDNS), optionally widened by the operator to a trusted
/// private split-DNS domain (e.g. `.int.data-zoo.de`) via
/// `TRITON_EGRESS_ALLOWED_SUFFIXES`. An arbitrary hostname could resolve to
/// a public or metadata IP, and non-canonical numeric forms
/// (octal/hex/decimal) that `IpAddr` won't parse must not slip through the
/// hostname path either. Public and link-local targets — notably
/// `169.254.169.254` cloud metadata — are refused. (Was `.consul` before the
/// Kamal migration; Codex review.)
///
/// Suffix matching is case-insensitive and ignores a trailing dot on the
/// host (the FQDN root). No DNS resolution happens here — the check is purely
/// name-suffix based, so there is no resolve-vs-connect TOCTOU window.
pub fn endpoint_is_dispatchable(endpoint: &str, allowed_suffixes: &[String]) -> bool {
    // Split off the port; tolerate a bracketed IPv6 host.
    let host = endpoint
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(endpoint);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        // Not an IP literal → only trust hostnames under an operator-allowed
        // DNS suffix (default `.ts.net`). This also rejects non-canonical IP
        // encodings (e.g. `0177.0.0.1`, `2130706433`) that `IpAddr` refuses
        // to parse.
        Err(_) => {
            let h = host.trim_end_matches('.').to_ascii_lowercase();
            allowed_suffixes
                .iter()
                .any(|suffix| h.ends_with(&suffix.to_ascii_lowercase()))
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
mod tests {
    use super::{MAX_SCOPES, endpoint_is_dispatchable, sanitise_scopes};
    use std::collections::HashSet;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The default egress policy: tailnet `.ts.net` only.
    fn default_suffixes() -> Vec<String> {
        s(&[".ts.net"])
    }

    #[test]
    fn sanitise_drops_junk_and_caps_count() {
        let long = "x".repeat(100);
        let mut input = s(&["chat", "has space", "reports"]);
        input.push(long); // over MAX_SCOPE_LEN
        input.push(String::new()); // empty
        // Pad well past the count cap with valid scopes.
        for i in 0..MAX_SCOPES + 10 {
            input.push(format!("extra{i}"));
        }
        let out = sanitise_scopes(&input, None);
        assert!(out.len() <= MAX_SCOPES, "count capped");
        assert!(out.contains(&"chat".to_string()));
        assert!(out.contains(&"reports".to_string()));
        assert!(!out.iter().any(|x| x.contains(' ')), "no whitespace scopes");
        assert!(!out.iter().any(|x| x.is_empty()), "no empty scopes");
        assert!(
            !out.iter().any(|x| x.len() > super::MAX_SCOPE_LEN),
            "no over-length"
        );
    }

    #[test]
    fn sanitise_applies_allowlist() {
        let allow: HashSet<String> = ["chat".to_string()].into_iter().collect();
        let out = sanitise_scopes(&s(&["chat", "admin"]), Some(&allow));
        assert_eq!(out, s(&["chat"]), "only allowlisted scopes survive");
    }

    #[test]
    fn sanitise_without_allowlist_keeps_clean_scopes() {
        let out = sanitise_scopes(&s(&["chat", "reports"]), None);
        assert_eq!(out, s(&["chat", "reports"]));
    }

    #[test]
    fn allows_loopback_private_and_tailnet_targets() {
        let p = default_suffixes();
        assert!(endpoint_is_dispatchable("127.0.0.1:8080", &p));
        assert!(endpoint_is_dispatchable("10.1.2.3:443", &p));
        assert!(endpoint_is_dispatchable("192.168.0.5:80", &p));
        assert!(endpoint_is_dispatchable("172.16.9.9:80", &p));
        assert!(endpoint_is_dispatchable("100.96.1.2:8001", &p)); // tailnet CGNAT
        assert!(endpoint_is_dispatchable("carl.dz.tailnet.ts.net:8001", &p)); // tailnet DNS
        assert!(endpoint_is_dispatchable("[::1]:8080", &p));
        assert!(endpoint_is_dispatchable("[fd7a:115c:a1e0::1]:8080", &p)); // ULA
    }

    #[test]
    fn refuses_public_and_metadata_targets() {
        let p = default_suffixes();
        assert!(!endpoint_is_dispatchable("169.254.169.254:80", &p)); // cloud metadata
        assert!(!endpoint_is_dispatchable("1.2.3.4:80", &p)); // public
        assert!(!endpoint_is_dispatchable("8.8.8.8:53", &p)); // public
        assert!(!endpoint_is_dispatchable("[2606:4700:4700::1111]:443", &p)); // public v6
    }

    #[test]
    fn refuses_arbitrary_hostnames_and_noncanonical_ip_encodings() {
        let p = default_suffixes();
        // An arbitrary hostname could resolve to a public/metadata IP, and
        // non-canonical numeric encodings don't parse as an IP — neither may
        // take the hostname path.
        assert!(!endpoint_is_dispatchable("evil.example:80", &p));
        assert!(!endpoint_is_dispatchable("metadata.google.internal:80", &p));
        assert!(!endpoint_is_dispatchable("0177.0.0.1:80", &p)); // octal 127.0.0.1
        assert!(!endpoint_is_dispatchable("2130706433:80", &p)); // decimal 127.0.0.1
        assert!(!endpoint_is_dispatchable("0x7f.0.0.1:80", &p)); // hex
        // A `.ts.net`-suffixed lookalike under an attacker domain is still
        // not a tailnet name.
        assert!(!endpoint_is_dispatchable("ts.net.evil.com:80", &p));
        // Trailing-dot + mixed case tailnet name is still accepted.
        assert!(endpoint_is_dispatchable("Carl.DZ.Tailnet.TS.NET.:8001", &p));
    }

    #[test]
    fn rejects_private_dns_suffix_under_default_policy() {
        // The substrate's split-DNS domain is NOT trusted unless the operator
        // opts in — default policy is `.ts.net` only.
        let p = default_suffixes();
        assert!(!endpoint_is_dispatchable(
            "carl.nonprod.int.data-zoo.de:8001",
            &p
        ));
    }

    #[test]
    fn accepts_private_dns_suffix_when_operator_opts_in() {
        // With the suffix added to the policy, the same host is dispatchable;
        // `.ts.net` keeps working alongside it.
        let p = s(&[".ts.net", ".int.data-zoo.de"]);
        assert!(endpoint_is_dispatchable(
            "carl.nonprod.int.data-zoo.de:8001",
            &p
        ));
        assert!(endpoint_is_dispatchable(
            "escurel.nonprod.int.data-zoo.de:443",
            &p
        ));
        assert!(endpoint_is_dispatchable("carl.dz.tailnet.ts.net:8001", &p));
        // Case-insensitive + trailing-dot still hold for the added suffix.
        assert!(endpoint_is_dispatchable(
            "Carl.NONPROD.INT.DATA-ZOO.DE.:8001",
            &p
        ));
    }

    #[test]
    fn widening_the_policy_does_not_loosen_other_rules() {
        // A public host stays rejected even with a private suffix allowed,
        // and the IP-literal rules are untouched by the suffix list.
        let p = s(&[".ts.net", ".int.data-zoo.de"]);
        assert!(!endpoint_is_dispatchable("evil.example.com:80", &p));
        assert!(!endpoint_is_dispatchable("169.254.169.254:80", &p)); // metadata
        assert!(!endpoint_is_dispatchable("1.2.3.4:80", &p)); // public IP
        assert!(endpoint_is_dispatchable("127.0.0.1:8080", &p)); // loopback
        assert!(endpoint_is_dispatchable("10.1.2.3:443", &p)); // RFC-1918
        assert!(endpoint_is_dispatchable("100.96.1.2:8001", &p)); // CGNAT
        // A lookalike under an attacker domain is still not a match.
        assert!(!endpoint_is_dispatchable("int.data-zoo.de.evil.com:80", &p));
    }
}
