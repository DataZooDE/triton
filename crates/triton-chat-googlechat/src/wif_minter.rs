//! Keyless Google OAuth minter via Workload Identity Federation.
//!
//! The third leg of the outbound-credential split (T1a static bearer,
//! T1b service-account key, this = **no key at all**): the pod's
//! projected Kubernetes ServiceAccount OIDC token is exchanged for a
//! Google access token in two hops, each addressed by the standard
//! **external-account credential JSON** (the file
//! `gcloud iam workload-identity-pools create-cred-config` emits —
//! configuration, not secret material; the only secret involved is the
//! short-lived k8s token the kubelet rotates on its own):
//!
//!   1. `token_url` (STS, `sts.googleapis.com/v1/token`):
//!      RFC 8693 token exchange — the k8s JWT + the WIF pool `audience`
//!      → a federated access token, scope `cloud-platform` (per
//!      AIP-4117: the *customer* scope goes on hop 2, not here).
//!   2. `service_account_impersonation_url`
//!      (`iamcredentials …:generateAccessToken`): the federated token
//!      impersonates the Chat-app courier SA with scope `chat.bot` —
//!      the identity Google Chat attributes app messages to. A raw
//!      federated principal cannot call `chat.googleapis.com`; only a
//!      service account can, which is why the impersonation hop is not
//!      optional.
//!
//! Cache discipline mirrors `token_minter`: one token per process,
//! single-flight refresh under the lock, refresh-lead before expiry,
//! failures leave the cache untouched so the next use retries. The
//! k8s token file is re-read on every mint — the kubelet rotates it.
//!
//! Security: the k8s JWT, the federated token and the SA access token
//! are logged at NO level, and the k8s token never rides as a bearer
//! to anything but STS. Proven live 2026-08-30 (substrate#635 P0): an
//! impersonated project SA with scope `chat.bot` posts into a real
//! space AS the app and can `messages.patch` in place.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::token_minter::MinterError;

const CHAT_BOT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";
/// AIP-4117: with SA impersonation, STS gets the broad platform scope
/// and the customer scope is requested from `generateAccessToken`.
const STS_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_LEAD_SECS: u64 = 60;
/// Fallback lifetime when `generateAccessToken` answers without a
/// parseable `expireTime` (it always sends one; defensive only).
const FALLBACK_LIFETIME_SECS: u64 = 600;

/// Does a resolved `outbound.token` value look like an
/// external-account credential file? `true` routes to this minter
/// (strict parse, fail-closed); `false` falls through to the
/// SA-key / static-bearer checks.
pub fn looks_like_external_account_key(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "external_account")
        })
        .unwrap_or(false)
}

/// The external-account credential fields the two hops need.
#[derive(Debug, Deserialize)]
struct ExternalAccountKey {
    /// WIF pool/provider resource the STS exchange is scoped to.
    audience: String,
    /// e.g. `urn:ietf:params:oauth:token-type:jwt` for a k8s SA token.
    subject_token_type: String,
    /// STS endpoint.
    token_url: String,
    /// `iamcredentials …/serviceAccounts/{sa}:generateAccessToken`.
    service_account_impersonation_url: String,
    credential_source: CredentialSource,
}

#[derive(Debug, Deserialize)]
struct CredentialSource {
    /// Path of the projected k8s ServiceAccount token.
    file: String,
}

#[derive(Debug, Deserialize)]
struct StsResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImpersonationResponse {
    access_token: String,
    /// RFC 3339, e.g. `2026-08-30T12:00:00Z`.
    #[serde(default)]
    expire_time: Option<String>,
}

/// Cached, auto-refreshing WIF minter. One per adapter, shared by
/// every courier task through the adapter `Arc`.
pub struct WifMinter {
    key: ExternalAccountKey,
    http: reqwest::Client,
    cache: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    refresh_at: Instant,
}

// Manual Debug: never render the cache (it holds a live access token);
// the credential fields are configuration, safe to show.
impl std::fmt::Debug for WifMinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WifMinter")
            .field("audience", &self.key.audience)
            .field(
                "service_account_impersonation_url",
                &self.key.service_account_impersonation_url,
            )
            .finish_non_exhaustive()
    }
}

impl WifMinter {
    /// Strict parse of the resolved `outbound.token` value; any
    /// missing field is an error so boot fails closed instead of
    /// spawning courier tasks that can never deliver.
    pub fn from_credential_json(raw: &str) -> Result<Self, MinterError> {
        let key: ExternalAccountKey =
            serde_json::from_str(raw).map_err(|e| MinterError::Key(e.to_string()))?;
        for (name, v) in [
            ("audience", &key.audience),
            ("subject_token_type", &key.subject_token_type),
            ("token_url", &key.token_url),
            (
                "service_account_impersonation_url",
                &key.service_account_impersonation_url,
            ),
            ("credential_source.file", &key.credential_source.file),
        ] {
            if v.trim().is_empty() {
                return Err(MinterError::Key(format!("{name} is empty")));
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MinterError::Key(format!("http client: {e}")))?;
        Ok(Self {
            key,
            http,
            cache: Mutex::new(None),
        })
    }

    /// Return a valid SA access token (scope `chat.bot`), running the
    /// two-hop exchange when the cache is empty or near expiry.
    /// Single-flight under the lock, like `TokenMinter`.
    pub async fn access_token(&self) -> Result<String, MinterError> {
        let mut guard = self.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && Instant::now() < c.refresh_at
        {
            return Ok(c.access_token.clone());
        }
        // Re-read per mint: the kubelet rotates the projected token.
        let subject_token = std::fs::read_to_string(&self.key.credential_source.file)
            .map_err(|e| {
                MinterError::Key(format!(
                    "credential_source.file `{}`: {e}",
                    self.key.credential_source.file
                ))
            })?
            .trim()
            .to_string();

        // Hop 1 — STS token exchange: k8s JWT → federated access token.
        let sts_form = [
            ("grant_type", TOKEN_EXCHANGE_GRANT),
            ("audience", self.key.audience.as_str()),
            ("scope", STS_SCOPE),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("subject_token", subject_token.as_str()),
            ("subject_token_type", self.key.subject_token_type.as_str()),
        ];
        let resp = self
            .http
            .post(&self.key.token_url)
            .form(&sts_form)
            .send()
            .await
            .map_err(|e| MinterError::Transport(format!("sts: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(MinterError::Status(status));
        }
        let federated: StsResponse = resp
            .json()
            .await
            .map_err(|e| MinterError::Decode(format!("sts: {e}")))?;

        // Hop 2 — impersonate the courier SA with the customer scope.
        let resp = self
            .http
            .post(&self.key.service_account_impersonation_url)
            .bearer_auth(&federated.access_token)
            .json(&serde_json::json!({ "scope": [CHAT_BOT_SCOPE] }))
            .send()
            .await
            .map_err(|e| MinterError::Transport(format!("iamcredentials: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(MinterError::Status(status));
        }
        let minted: ImpersonationResponse = resp
            .json()
            .await
            .map_err(|e| MinterError::Decode(format!("iamcredentials: {e}")))?;

        let lifetime = minted
            .expire_time
            .as_deref()
            .and_then(expire_time_to_lifetime_secs)
            .unwrap_or(FALLBACK_LIFETIME_SECS);
        let refresh_at =
            Instant::now() + Duration::from_secs(lifetime.saturating_sub(REFRESH_LEAD_SECS).max(1));
        let token = minted.access_token.clone();
        *guard = Some(CachedToken {
            access_token: minted.access_token,
            refresh_at,
        });
        Ok(token)
    }
}

/// Seconds from now until an RFC 3339 `expireTime`, `None` when the
/// stamp doesn't parse or is already past. chrono is what the msteams
/// twin already uses for the same job.
fn expire_time_to_lifetime_secs(stamp: &str) -> Option<u64> {
    let expires = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    let secs = (expires.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    (secs > 0).then_some(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_key() -> String {
        serde_json::json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/x",
            "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
            "token_url": "https://sts.googleapis.com/v1/token",
            "service_account_impersonation_url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken",
            "credential_source": { "file": "/var/run/secrets/gcp/token" }
        })
        .to_string()
    }

    #[test]
    fn detection_requires_external_account_type() {
        assert!(looks_like_external_account_key(&valid_key()));
        assert!(!looks_like_external_account_key(
            r#"{"type":"service_account","client_email":"a@b","private_key":"x"}"#
        ));
        assert!(!looks_like_external_account_key("static-bearer-token"));
    }

    #[test]
    fn a_valid_credential_parses() {
        WifMinter::from_credential_json(&valid_key()).expect("parses");
    }

    #[test]
    fn missing_impersonation_url_fails_closed() {
        // Direct-access external accounts (no impersonation) exist in
        // GCP, but cannot call chat.googleapis.com — only a service
        // account can. Refusing at boot beats couriers that can never
        // deliver.
        let mut v: Value = serde_json::from_str(&valid_key()).unwrap();
        v.as_object_mut()
            .unwrap()
            .remove("service_account_impersonation_url");
        let err = WifMinter::from_credential_json(&v.to_string()).expect_err("must fail");
        assert!(matches!(err, MinterError::Key(_)), "{err}");
    }

    #[test]
    fn empty_fields_fail_closed() {
        let mut v: Value = serde_json::from_str(&valid_key()).unwrap();
        v["token_url"] = serde_json::json!("  ");
        let err = WifMinter::from_credential_json(&v.to_string()).expect_err("must fail");
        assert!(matches!(err, MinterError::Key(_)), "{err}");
    }

    #[test]
    fn expire_time_parses_to_lifetime() {
        let future = (chrono::Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339();
        let secs = expire_time_to_lifetime_secs(&future).expect("parses");
        assert!((3590..=3600).contains(&secs), "{secs}");
        assert_eq!(expire_time_to_lifetime_secs("garbage"), None);
    }
}
