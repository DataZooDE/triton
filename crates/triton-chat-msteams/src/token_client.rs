//! Bot Framework outbound token client.
//!
//! The Microsoft Bot Connector requires an OAuth2 access token on
//! every outbound Activity POST. The token is minted by
//! `login.microsoftonline.com` using the client_credentials grant
//! and lasts ~1 hour. Two credential modes prove the client:
//!
//! * [`Credential::Secret`] — a `client_secret` (resolved at boot
//!   from Vault per FR-L-6), against the multi-tenant
//!   `/botframework.com/` token endpoint.
//! * [`Credential::Federated`] — a signed JWT read from a file, sent
//!   as `client_assertion` (RFC 7523). This is what a Kubernetes pod
//!   holding an Entra *federated credential* uses: the projected
//!   ServiceAccount token at `AZURE_FEDERATED_TOKEN_FILE` is the
//!   assertion, so the deployment holds NO static secret at all.
//!   Federated bots are single-tenant, so this mode targets the
//!   tenant-scoped `/{tenant_id}/` token endpoint — the
//!   `botframework.com` endpoint does not know our federated
//!   credential.
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
//! * The federated assertion is re-read from disk on EVERY refresh,
//!   never cached: kubelet rotates the projected token (~hourly, and
//!   sooner near expiry), so a value cached at boot goes stale and
//!   the grant starts failing an hour in — the kind of fault that
//!   only shows up in production.
//! * NFR-S-4 holds in both modes: the token HOST is hardcoded. The
//!   federated mode interpolates only a tenant id, and validates it
//!   as an opaque `[A-Za-z0-9.-]` segment first, so no configuration
//!   can redirect the grant at another host.
//! * NFR-S-4: the token URL is hardcoded. Operators cannot point
//!   the outbound auth at an attacker-controlled host.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

/// Hardcoded Microsoft login host (NFR-S-4 egress allowlist).
/// Operators get NO knob to override this; the substrate ACL only
/// permits `login.microsoftonline.com`.
const TOKEN_HOST: &str = "https://login.microsoftonline.com";

/// Hardcoded Microsoft Bot Framework token endpoint for the
/// client_secret (multi-tenant) mode.
const TOKEN_URL: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";

/// RFC 7523 assertion type for the federated (client_assertion)
/// grant. Microsoft requires this exact string.
const CLIENT_ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

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
    credential: Credential,
    token_url: String,
    http: reqwest::Client,
    cache: Mutex<Option<CachedToken>>,
}

/// How this client proves it is the bot to Microsoft.
enum Credential {
    /// A static `client_secret`.
    Secret(String),
    /// A file holding a signed JWT to present as `client_assertion`
    /// — the projected ServiceAccount token of a pod with an Entra
    /// federated credential. Re-read on every refresh.
    Federated { token_file: PathBuf },
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
    #[error("federated token file {0}: {1}")]
    CredentialRead(String, String),
    #[error("invalid tenant id: {0}")]
    InvalidTenant(String),
}

impl TokenClient {
    /// Production constructor, client_secret mode — points at
    /// Microsoft's hardcoded multi-tenant token endpoint. NFR-S-4:
    /// no override path.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self::with_token_url(client_id, client_secret, TOKEN_URL)
    }

    /// Production constructor, federated mode — no static secret.
    /// `token_file` is the projected ServiceAccount token the pod
    /// presents as `client_assertion` (in Kubernetes:
    /// `AZURE_FEDERATED_TOKEN_FILE`).
    ///
    /// The endpoint is tenant-scoped because an Entra federated
    /// credential is registered on an app in OUR tenant; the shared
    /// `botframework.com` endpoint cannot verify it. Only the tenant
    /// segment is interpolated, and only after it validates as
    /// `[A-Za-z0-9.-]+` — the host stays hardcoded (NFR-S-4).
    pub fn with_federated_credential(
        client_id: impl Into<String>,
        tenant_id: &str,
        token_file: impl Into<PathBuf>,
    ) -> Result<Self, TokenError> {
        let tenant = validate_tenant_id(tenant_id)?;
        Ok(Self::build(
            client_id,
            Credential::Federated {
                token_file: token_file.into(),
            },
            format!("{TOKEN_HOST}/{tenant}/oauth2/v2.0/token"),
        ))
    }

    /// Test constructor — lets the integration fixture point the
    /// client at its own `FakeBotFramework` instance. NOT exposed
    /// to operators; only the test fixture wires this.
    pub fn with_token_url(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        Self::build(
            client_id,
            Credential::Secret(client_secret.into()),
            token_url.into(),
        )
    }

    /// Test constructor for the federated grant. Same
    /// not-for-operators rule as [`Self::with_token_url`].
    pub fn with_federated_token_url(
        client_id: impl Into<String>,
        token_file: impl Into<PathBuf>,
        token_url: impl Into<String>,
    ) -> Self {
        Self::build(
            client_id,
            Credential::Federated {
                token_file: token_file.into(),
            },
            token_url.into(),
        )
    }

    fn build(client_id: impl Into<String>, credential: Credential, token_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("reqwest client builds with valid options");
        Self {
            client_id: client_id.into(),
            credential,
            token_url,
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
        // Re-read the assertion on every refresh: kubelet rotates
        // the projected token, so anything cached at boot goes stale.
        let assertion = match &self.credential {
            Credential::Secret(_) => None,
            Credential::Federated { token_file } => Some(read_assertion(token_file).await?),
        };
        let mut body: Vec<(&str, &str)> = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("scope", SCOPE),
        ];
        match (&self.credential, assertion.as_deref()) {
            (Credential::Secret(secret), _) => body.push(("client_secret", secret.as_str())),
            (Credential::Federated { .. }, Some(a)) => {
                body.push(("client_assertion_type", CLIENT_ASSERTION_TYPE));
                body.push(("client_assertion", a));
            }
            // Unreachable: the match above sets `assertion` for
            // exactly the Federated arm.
            (Credential::Federated { .. }, None) => {
                return Err(TokenError::CredentialRead(
                    "<unset>".into(),
                    "federated assertion missing".into(),
                ));
            }
        }
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

/// Read the federated assertion from disk. Trimmed because a
/// projected token file has no trailing newline but a hand-made test
/// or a `kubectl cp` easily does, and a stray `\n` makes Entra
/// reject the assertion with an opaque `AADSTS700027`.
async fn read_assertion(path: &Path) -> Result<String, TokenError> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| TokenError::CredentialRead(path.display().to_string(), e.to_string()))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(TokenError::CredentialRead(
            path.display().to_string(),
            "file is empty".into(),
        ));
    }
    Ok(trimmed)
}

/// A tenant id is interpolated into the token URL, so it is checked
/// as an opaque segment before use: a GUID or a verified domain, and
/// nothing that could escape the path or swap the host.
fn validate_tenant_id(tenant_id: &str) -> Result<&str, TokenError> {
    let t = tenant_id.trim();
    if t.is_empty()
        || !t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(TokenError::InvalidTenant(tenant_id.to_string()));
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_id_accepts_guid_and_domain() {
        assert!(validate_tenant_id("28c0071d-815c-4ace-a3b5-9a28bde005fd").is_ok());
        assert!(validate_tenant_id("data-zoo.de").is_ok());
        // Trimmed, not rejected — trailing whitespace from a config
        // file is an operator typo, not an attack.
        assert_eq!(validate_tenant_id("  tenant-1  ").unwrap(), "tenant-1");
    }

    #[test]
    fn tenant_id_rejects_anything_that_could_escape_the_url() {
        // NFR-S-4: none of these may reach the URL builder. A `/`
        // would append a path, `@` would swap the authority, and a
        // full URL would move the grant to another host entirely.
        for bad in [
            "",
            "   ",
            "../../evil",
            "tenant/../../x",
            "evil.com/path",
            "user@evil.com",
            "tenant?x=1",
            "tenant#frag",
            "https://evil.com",
            "tenant id",
        ] {
            assert!(
                validate_tenant_id(bad).is_err(),
                "tenant id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn federated_client_targets_the_tenant_endpoint_on_the_hardcoded_host() {
        let c = TokenClient::with_federated_credential(
            "client-1",
            "28c0071d-815c-4ace-a3b5-9a28bde005fd",
            "/var/run/secrets/azure/token",
        )
        .expect("valid tenant builds");
        assert_eq!(
            c.token_url,
            "https://login.microsoftonline.com/28c0071d-815c-4ace-a3b5-9a28bde005fd/oauth2/v2.0/token"
        );
        assert!(c.token_url.starts_with(TOKEN_HOST));
    }

    #[test]
    fn federated_client_refuses_a_hostile_tenant_id() {
        // Deliberately not `expect_err`: that needs `Debug` on
        // TokenClient, and TokenClient holds the client_secret — a
        // derived Debug is exactly how secrets end up in logs.
        match TokenClient::with_federated_credential("client-1", "evil.com/x", "/tmp/t") {
            Err(TokenError::InvalidTenant(t)) => assert_eq!(t, "evil.com/x"),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("hostile tenant id must be refused"),
        }
    }

    #[test]
    fn secret_client_keeps_the_multi_tenant_endpoint() {
        let c = TokenClient::new("client-1", "shhh");
        assert_eq!(c.token_url, TOKEN_URL);
    }

    #[tokio::test]
    async fn assertion_is_trimmed_of_a_trailing_newline() {
        // A projected token has no trailing newline; a hand-written
        // one usually does, and Entra rejects the padded assertion
        // with an opaque AADSTS700027.
        let dir = std::env::temp_dir().join(format!("triton-fed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "header.payload.sig\n").unwrap();
        assert_eq!(read_assertion(&path).await.unwrap(), "header.payload.sig");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_or_empty_assertion_file_is_a_named_error() {
        let missing = std::path::Path::new("/nonexistent/triton/federated/token");
        assert!(matches!(
            read_assertion(missing).await,
            Err(TokenError::CredentialRead(_, _))
        ));

        let dir = std::env::temp_dir().join(format!("triton-fed-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        // An empty file is what a not-yet-projected volume looks
        // like; failing loudly beats sending `client_assertion=`.
        std::fs::write(&path, "   \n").unwrap();
        assert!(matches!(
            read_assertion(&path).await,
            Err(TokenError::CredentialRead(_, _))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
