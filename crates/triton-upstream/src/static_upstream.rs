//! Dev-only static upstream dispatch (issue #75, Mode 2).
//!
//! Resolve a tool to a fixed `host:port` from a static map and POST the
//! args there with a static bearer — **no Consul, no Vault**. For local
//! "standalone sidecar" dev where one Triton fronts a single agent; the
//! production path is [`crate::UpstreamRouter`]. Gate behind dev usage
//! (set via `TRITON_STATIC_UPSTREAMS` in `triton-bin`).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use triton_core::{Principal, TritonError, UpstreamDispatch};

pub struct StaticUpstream {
    map: HashMap<String, String>,
    token: String,
    http: reqwest::Client,
}

impl StaticUpstream {
    /// Parse `name=host:port,name2=host:port` into the static map. The
    /// `token` is sent as the upstream bearer (default `dev-token`, which
    /// the dev agent accepts).
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
        Self { map, token, http }
    }
}

#[async_trait]
impl UpstreamDispatch for StaticUpstream {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _principal: &Principal,
    ) -> Result<Value, TritonError> {
        let ep = self
            .map
            .get(tool)
            .ok_or_else(|| TritonError::Validation(format!("unknown tool: {tool}")))?;
        let resp = self
            .http
            .post(format!("http://{ep}/"))
            .bearer_auth(&self.token)
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
