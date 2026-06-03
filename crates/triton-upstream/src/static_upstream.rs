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
//!     the Consul-less analogue of [`crate::UpstreamRouter`]'s Vault swap.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use triton_core::{Principal, TritonError, UpstreamDispatch};
use triton_identity::JwtSigner;

/// Upstream OIDC token TTL (NFR-S-3 cap is enforced by the signer too).
const TOKEN_TTL: Duration = Duration::from_secs(300);

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
    ) -> Self {
        self.signer = Some(signer);
        self.audience = audience.into();
        self.tenant = tenant.into();
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
                s.sign(&auds, &principal.sub, &self.tenant, TOKEN_TTL)
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
