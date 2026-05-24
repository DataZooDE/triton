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
}
