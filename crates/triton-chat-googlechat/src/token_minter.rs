//! #164 T1b — Google service-account OAuth token minter.
//!
//! Posting to `chat.googleapis.com` needs an OAuth2 access token with
//! scope `https://www.googleapis.com/auth/chat.bot`, minted from the
//! Chat app's service account via the **JWT-bearer grant**: sign an
//! assertion JWT (RS256, the SA key's `private_key`; claims
//! `iss = client_email`, `scope = chat.bot`, `aud = the token URL`,
//! `iat`/`exp` ~1h) and exchange it at the key's `token_uri`
//! (canonically `oauth2.googleapis.com/token`) with
//! `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`.
//!
//! Mirrors the msteams `TokenClient` precedent: one cached in-memory
//! token per process; the cache lock is held for the duration of any
//! refresh so a thundering herd at expiry hits Google once
//! (single-flight); we refresh shortly before the announced expiry. A
//! failed refresh surfaces as an error the courier audits as a
//! retryable delivery failure — the cache stays empty and the next
//! use re-attempts; nothing panics.
//!
//! Security notes:
//! * The access token and the SA private key are logged at NO level.
//! * The token URL comes from the SA key itself (`token_uri`), which
//!   is operator-managed secret material (a Vault ref in prod) — the
//!   same trust class as the msteams client_secret. Tests point it at
//!   a local fake by authoring the key; there is no separate env knob.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// The OAuth scope Google Chat's create-message REST call requires.
const CHAT_BOT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";

/// Canonical Google OAuth2 token endpoint — the fallback when the SA
/// key carries no `token_uri` (real Google key files always do).
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// RFC 7523 JWT-bearer grant type.
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Per-call HTTP timeout for the token exchange. Aggressive because a
/// hung token endpoint shouldn't wedge the courier task.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Refresh the cached access token this many seconds before its
/// announced expiry, so a slow exchange never collides with the cliff.
const REFRESH_LEAD_SECS: u64 = 60;

/// Assertion JWT lifetime — Google caps it at one hour.
const ASSERTION_LIFETIME_SECS: u64 = 3600;

/// Cheap shape test for the T1a/T1b mode split: does a resolved
/// `outbound.token` value look like a standard Google service-account
/// key file (a JSON object with `"type": "service_account"`)? `true`
/// routes to the minter (which then parses strictly and fails closed
/// on a broken key); `false` keeps the T1a static-bearer path — the
/// deliberate operator escape hatch.
pub fn looks_like_service_account_key(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "service_account")
        })
        .unwrap_or(false)
}

/// The fields of the standard downloaded SA key file the minter needs.
#[derive(Debug, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    /// PKCS#8 RSA private key PEM.
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MinterError {
    /// The key JSON is SA-shaped but unusable (missing fields, empty
    /// client_email, a private_key that isn't a parseable RSA PEM).
    /// Boot maps this to a fail-closed refusal.
    #[error("service-account key: {0}")]
    Key(String),
    #[error("assertion signing: {0}")]
    Sign(String),
    #[error("token endpoint transport: {0}")]
    Transport(String),
    #[error("token endpoint returned status {0}")]
    Status(u16),
    #[error("token endpoint body decode: {0}")]
    Decode(String),
}

/// Cached, auto-refreshing SA token minter. One per adapter; shared by
/// every courier task through the adapter `Arc`.
pub struct TokenMinter {
    client_email: String,
    signing_key: jsonwebtoken::EncodingKey,
    token_url: String,
    http: reqwest::Client,
    cache: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    refresh_at: Instant,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until expiry per the OAuth2 spec.
    expires_in: u64,
}

impl TokenMinter {
    /// Production constructor — parse the resolved `outbound.token`
    /// value as an SA key file. Token URL precedence: the key's
    /// `token_uri` > [`DEFAULT_TOKEN_URL`]. Strict: any missing/broken
    /// field is an error so boot fails closed instead of spawning
    /// courier tasks that can never deliver.
    pub fn from_key_json(raw: &str) -> Result<Self, MinterError> {
        Self::with_token_url(raw, None)
    }

    /// Test constructor — lets a harness override the token URL
    /// (precedence: explicit override > key's `token_uri` > default).
    /// NOT exposed to operators; the integration fixtures instead
    /// author the key's `token_uri`, exercising the production path.
    pub fn with_token_url(raw: &str, override_url: Option<&str>) -> Result<Self, MinterError> {
        let key: ServiceAccountKey =
            serde_json::from_str(raw).map_err(|e| MinterError::Key(e.to_string()))?;
        if key.client_email.trim().is_empty() {
            return Err(MinterError::Key("client_email is empty".into()));
        }
        let signing_key = jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .map_err(|e| MinterError::Key(format!("private_key is not a usable RSA PEM: {e}")))?;
        let token_url = override_url
            .map(str::to_string)
            .or_else(|| {
                key.token_uri
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string());
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MinterError::Key(format!("http client: {e}")))?;
        Ok(Self {
            client_email: key.client_email,
            signing_key,
            token_url,
            http,
            cache: Mutex::new(None),
        })
    }

    /// Return a valid access token, minting via the JWT-bearer grant
    /// when the cache is empty or within [`REFRESH_LEAD_SECS`] of
    /// expiry. The lock is held across the refresh (single-flight): a
    /// stampede of courier tasks at expiry performs one exchange.
    pub async fn access_token(&self) -> Result<String, MinterError> {
        let mut guard = self.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && Instant::now() < c.refresh_at
        {
            return Ok(c.access_token.clone());
        }
        let assertion = self.sign_assertion()?;
        let body = [
            ("grant_type", JWT_BEARER_GRANT),
            ("assertion", assertion.as_str()),
        ];
        let resp = self
            .http
            .post(&self.token_url)
            .form(&body)
            .send()
            .await
            .map_err(|e| MinterError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // Leave the cache untouched (still empty / stale): the
            // next use retries the exchange instead of giving up.
            return Err(MinterError::Status(status));
        }
        let parsed: TokenResponse = resp
            .json()
            .await
            .map_err(|e| MinterError::Decode(e.to_string()))?;
        let refresh_at = Instant::now()
            + Duration::from_secs(parsed.expires_in.saturating_sub(REFRESH_LEAD_SECS).max(1));
        let token = parsed.access_token.clone();
        *guard = Some(CachedToken {
            access_token: parsed.access_token,
            refresh_at,
        });
        Ok(token)
    }

    /// Build + RS256-sign the assertion JWT for one exchange.
    fn sign_assertion(&self) -> Result<String, MinterError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| MinterError::Sign(e.to_string()))?
            .as_secs();
        let claims = serde_json::json!({
            "iss": self.client_email,
            "scope": CHAT_BOT_SCOPE,
            "aud": self.token_url,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|e| MinterError::Sign(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_requires_json_object_with_service_account_type() {
        assert!(looks_like_service_account_key(
            r#"{"type":"service_account","client_email":"a@b","private_key":"x"}"#
        ));
        // Static bearers, non-JSON, wrong type, non-object → T1a path.
        assert!(!looks_like_service_account_key("static-bearer-token"));
        assert!(!looks_like_service_account_key(
            r#"{"type":"authorized_user"}"#
        ));
        assert!(!looks_like_service_account_key(r#""service_account""#));
        assert!(!looks_like_service_account_key(r#"["service_account"]"#));
        assert!(!looks_like_service_account_key(""));
    }

    #[test]
    fn broken_private_key_is_a_key_error() {
        let raw = r#"{
            "type": "service_account",
            "client_email": "a@b.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\nnot a key\n-----END PRIVATE KEY-----\n"
        }"#;
        let Err(err) = TokenMinter::from_key_json(raw) else {
            panic!("must refuse a broken PEM");
        };
        assert!(matches!(err, MinterError::Key(_)), "{err}");
    }

    #[test]
    fn missing_client_email_is_a_key_error() {
        let raw = r#"{"type":"service_account","private_key":"x"}"#;
        assert!(matches!(
            TokenMinter::from_key_json(raw),
            Err(MinterError::Key(_))
        ));
    }
}
