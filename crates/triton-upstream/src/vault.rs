//! Vault per-call OIDC swap (FR-U-2, NFR-S-3). Mints a short-lived
//! identity token via Vault's `/v1/identity/oidc/token/<role>` plugin.

use std::time::Duration;

use serde::Deserialize;
use triton_secrets::VaultToken;

#[derive(Clone)]
pub struct VaultClient {
    base: String,
    token: VaultToken,
    http: reqwest::Client,
}

impl VaultClient {
    pub fn new(base_url: impl Into<String>, token: VaultToken) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Mint a fresh OIDC token under `role`. Returns the raw token
    /// string (a JWT in production; opaque in tests). Caller is
    /// responsible for treating the token as bearer-shaped and never
    /// logging it.
    pub async fn mint_oidc(&self, role: &str) -> Result<String, String> {
        let url = format!("{}/v1/identity/oidc/token/{role}", self.base);
        let vault_token = self
            .token
            .get()
            .await
            .map_err(|e| format!("vault auth: {e}"))?;
        let resp: TokenResponse = self
            .http
            .get(&url)
            .header("X-Vault-Token", &vault_token)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("GET {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("decode {url}: {e}"))?;
        if resp.data.ttl > 300 {
            // NFR-S-3 sanity: TTL ≤ 5 min. Vault role config
            // SHOULD enforce this; reject locally if the issuer
            // misconfigured.
            return Err(format!(
                "vault returned a token with ttl={}s, exceeds NFR-S-3 cap of 300s",
                resp.data.ttl
            ));
        }
        Ok(resp.data.token)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    data: TokenData,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    token: String,
    /// Required. `#[serde(default)]` would silently let a missing
    /// `ttl` field pass the NFR-S-3 cap as `0` — fail closed instead.
    ttl: u64,
}
