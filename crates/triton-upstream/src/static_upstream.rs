//! Static upstream dispatch (issue #75, Mode 2): resolve a tool to a fixed
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use triton_core::{Principal, TritonError, UpstreamDispatch};
use triton_identity::JwtSigner;

/// Upstream OIDC token TTL (NFR-S-3 cap is enforced by the signer too).
const TOKEN_TTL: Duration = Duration::from_secs(300);

/// #114 caps on resolver-supplied principal data forwarded into the
/// minted token, so a buggy/hostile resolver can't bloat or corrupt it.
const MAX_SCOPES: usize = 32;
const MAX_SCOPE_LEN: usize = 64;
const MAX_TENANT_LEN: usize = 128;

/// Sanitise resolver-supplied scopes before they're signed into the
/// `triton_sender_scopes` claim (#114): drop empty / whitespace-bearing /
/// over-length values, apply the operator allowlist when configured, and
/// cap the count. Pure so it's unit-testable.
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
}

impl StaticUpstream {
    /// Parse `name=host:port,name2=host:port` into the static map. The
    /// `token` is sent as the upstream bearer (default `dev-token`, which
    /// a dev-token agent accepts) unless a signer is attached.
    pub fn from_spec(spec: &str, token: String, timeout: Duration) -> Self {
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
    ) -> Self {
        self.signer = Some(signer);
        self.audience = audience.into();
        self.tenant = tenant.into();
        self.forward_principal = forward_principal;
        self.forward_scope_allowlist = forward_scope_allowlist;
        self
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
                let (tenant, scopes): (String, Vec<String>) = if self.forward_principal {
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
                    (tenant, scopes)
                } else {
                    (self.tenant.clone(), Vec::new())
                };
                s.sign(&auds, &principal.sub, &tenant, &scopes, TOKEN_TTL)
                    .map_err(|e| TritonError::Tool(format!("mint upstream token: {e}")))
            }
            None => Ok(self.token.clone()),
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
            .map_err(|e| TritonError::Tool(format!("upstream {tool} unreachable: {e}")))?;
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

    async fn list_agents(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SCOPES, sanitise_scopes};
    use std::collections::HashSet;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
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
}
