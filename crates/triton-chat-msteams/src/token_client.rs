//! Bot Framework outbound token client.
//!
//! The Microsoft Bot Connector requires an OAuth2 access token on
//! every outbound Activity POST. The token is minted by
//! `login.microsoftonline.com/botframework.com/oauth2/v2.0/token`
//! using the client_credentials grant against the bot's
//! `client_id` / `client_secret` (resolved at boot from Vault per
//! FR-L-6) and lasts ~1 hour.
//!
//! Cache shape: one in-memory token per process. The lock holds for
//! the duration of any refresh so a thundering herd at expiry only
//! hits Microsoft once. We refresh ~5 min before the announced
//! expiry so a slow refetch never collides with the cliff.
//!
//! Security notes:
//! * The access token is logged at NO level — `tracing` calls in
//!   this module never include the bearer.
//! * The client_secret is held only as a `String` field; it never
//!   appears in errors (the token endpoint never echoes it).
//! * NFR-S-4: the token URL is hardcoded. Operators cannot point
//!   the outbound auth at an attacker-controlled host.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

/// Hardcoded Microsoft Bot Framework token endpoint (NFR-S-4
/// egress allowlist). Operators get NO knob to override this; the
/// substrate ACL only permits `login.microsoftonline.com`.
const TOKEN_URL: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";

/// Per-call HTTP timeout for the token fetch. Aggressive because a
/// hung token endpoint shouldn't block the inbound webhook handler.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Refresh the cached access token this many seconds before its
/// announced expiry. Five minutes covers a slow Microsoft response
/// without ever serving a token that's already-or-about-to-be
/// rejected.
const REFRESH_LEAD_SECS: u64 = 300;

/// Scope passed in the client_credentials grant. The trailing
/// `.default` is the Microsoft convention for "all scopes the app
/// is permitted to call".
const SCOPE: &str = "https://api.botframework.com/.default";

/// One outbound token client. Holds the bot's credentials in memory
/// and a cached access token; refreshes on cache miss or imminent
/// expiry.
pub struct TokenClient {
    client_id: String,
    client_secret: String,
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
    /// Seconds until expiry per the OAuth2 client_credentials spec.
    expires_in: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("token endpoint transport: {0}")]
    Transport(String),
    #[error("token endpoint returned status {0}")]
    Status(u16),
    #[error("token endpoint body decode: {0}")]
    Decode(String),
}

impl TokenClient {
    /// Production constructor — points at Microsoft's hardcoded
    /// token endpoint. NFR-S-4: no override path.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self::with_token_url(client_id, client_secret, TOKEN_URL)
    }

    /// Test constructor — lets the integration fixture point the
    /// client at its own `FakeBotFramework` instance. NOT exposed
    /// to operators; only the test fixture wires this.
    pub fn with_token_url(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("reqwest client builds with valid options");
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token_url: token_url.into(),
            http,
            cache: Mutex::new(None),
        }
    }

    /// Return a valid access token. Refreshes from the token
    /// endpoint when the cache is empty or within
    /// [`REFRESH_LEAD_SECS`] of expiry.
    pub async fn access_token(&self) -> Result<String, TokenError> {
        let mut guard = self.cache.lock().await;
        if let Some(c) = guard.as_ref()
            && Instant::now() < c.refresh_at
        {
            return Ok(c.access_token.clone());
        }
        let body = [
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", SCOPE),
        ];
        let resp = self
            .http
            .post(&self.token_url)
            .form(&body)
            .send()
            .await
            .map_err(|e| TokenError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(TokenError::Status(status));
        }
        let parsed: TokenResponse = resp
            .json()
            .await
            .map_err(|e| TokenError::Decode(e.to_string()))?;
        let refresh_at = Instant::now()
            + Duration::from_secs(parsed.expires_in.saturating_sub(REFRESH_LEAD_SECS).max(1));
        let token = parsed.access_token.clone();
        *guard = Some(CachedToken {
            access_token: parsed.access_token,
            refresh_at,
        });
        Ok(token)
    }
}
