//! Consul agent discovery (FR-U-1). Speaks the Consul HTTP API's
//! `/v1/health/service/<service>?passing` endpoint.

use std::time::Duration;

use crate::ConsulServiceEntry;

#[derive(Clone)]
pub struct ConsulClient {
    base: String,
    http: reqwest::Client,
}

impl ConsulClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Resolve `tool` to a single healthy upstream endpoint
    /// (`<address>:<port>`). Returns `None` if no service registered
    /// or none carry the FR-U-1 `agent:<tool>` tag.
    pub async fn resolve(&self, tool: &str) -> Result<Option<String>, String> {
        let url = format!("{}/v1/health/service/{tool}?passing", self.base);
        let entries: Vec<ConsulServiceEntry> = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("GET {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("decode {url}: {e}"))?;
        let required_tag = format!("agent:{tool}");
        Ok(entries
            .into_iter()
            .find(|e| e.service.tags.iter().any(|t| t == &required_tag))
            .map(|e| format!("{}:{}", e.service.address, e.service.port)))
    }

    /// List the tool names of every registered `agent:<name>` service
    /// (FR-U-1 discovery, listing direction). Reads Consul's
    /// `/v1/catalog/services` — a `{ "<service>": ["<tag>", ...] }` map —
    /// and extracts `<name>` from every `agent:<name>` tag. The tool name
    /// lives in the *tag*, not the service name, so this is independent
    /// of how the operator named the Nomad service. De-duplicated and
    /// sorted. Note: the catalog doesn't carry health, so this lists
    /// *registered* agents (a subsequent `resolve` still gates on
    /// `?passing`).
    pub async fn list_agent_tools(&self) -> Result<Vec<String>, String> {
        let url = format!("{}/v1/catalog/services", self.base);
        let map: std::collections::BTreeMap<String, Vec<String>> = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("GET {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("decode {url}: {e}"))?;
        let mut tools: Vec<String> = map
            .into_values()
            .flatten()
            .filter_map(|tag| tag.strip_prefix("agent:").map(str::to_string))
            .collect();
        tools.sort();
        tools.dedup();
        Ok(tools)
    }
}
