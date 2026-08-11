//! Shared Twilio Messaging REST courier — `POST
//! /2010-04-01/Accounts/{AccountSid}/Messages.json`, form-encoded, HTTP
//! Basic `AccountSid:AuthToken`. One client, reused by every Twilio
//! channel adapter (WhatsApp now; RCS in a follow-up PR) since Twilio
//! fans several channels out through this single Messaging API — only
//! the `To`/`From` prefix and message content differ per channel.
//!
//! Twilio's response: `201 Created` with `{"sid": "SM...", "status":
//! "queued", ...}` on success; a non-2xx with `{"code", "message",
//! "more_info", "status"}` on failure. We only need the HTTP status to
//! classify posted/retry/dropped — no response body parsing required.

use std::time::Duration;

/// Configuration for the outbound courier. `api_base` is Twilio's API
/// endpoint (env `TRITON_TWILIO_API_BASE`, pointed at the in-repo fake in
/// tests).
#[derive(Debug, Clone)]
pub struct CourierConfig {
    pub api_base: String,
    pub timeout: Duration,
}

impl Default for CourierConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.twilio.com".to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Classification driving the dispatcher's `post` audit `status_label`
/// (FR-AU-1 v0.2), same three-way split every other courier uses.
#[derive(Debug, Clone, Copy)]
pub enum PostLabel {
    Posted,
    Retry,
    Dropped,
}

impl PostLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Posted => "posted",
            Self::Retry => "retry",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug)]
pub struct SendOutcome {
    pub http_status: u16,
    pub label: PostLabel,
}

#[derive(Debug)]
pub enum CourierError {
    Transport(String),
    Application { http_status: u16, label: PostLabel },
}

impl CourierError {
    pub fn label(&self) -> PostLabel {
        match self {
            Self::Transport(_) => PostLabel::Retry,
            Self::Application { label, .. } => *label,
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Transport(_) => 0,
            Self::Application { http_status, .. } => *http_status,
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Transport(m) => format!("twilio courier transport: {m}"),
            Self::Application { http_status, label } => format!(
                "twilio courier application: http_status={http_status}, label={}",
                label.as_str()
            ),
        }
    }
}

/// Strip every occurrence of the Auth Token from a log/error string so a
/// stray transport-error `Display` can never leak the Basic-auth password
/// (FR-A-3 parity with every other courier's `redact`).
fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        return s.to_string();
    }
    s.replace(secret, "<redacted>")
}

pub struct TwilioCourierClient {
    base: String,
    http: reqwest::Client,
}

impl TwilioCourierClient {
    pub fn new(cfg: CourierConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("twilio courier http client: {e}"))?;
        Ok(Self {
            base: cfg.api_base.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// `POST /2010-04-01/Accounts/{account_sid}/Messages.json` with
    /// `params` as the form body. `params` MUST already include `From` /
    /// `To` / the message content field(s) the caller wants sent — this
    /// method is deliberately channel-agnostic (WhatsApp's `Body` today;
    /// RCS's richer content fields reuse the same POST later).
    pub async fn send_message(
        &self,
        account_sid: &str,
        auth_token: &str,
        params: &[(&str, &str)],
    ) -> Result<SendOutcome, CourierError> {
        let url = format!(
            "{}/2010-04-01/Accounts/{}/Messages.json",
            self.base, account_sid
        );
        let resp = self
            .http
            .post(&url)
            .basic_auth(account_sid, Some(auth_token))
            .form(params)
            .send()
            .await
            .map_err(|e| CourierError::Transport(redact(&e.to_string(), auth_token)))?;
        let http_status = resp.status().as_u16();
        if (200..300).contains(&http_status) {
            return Ok(SendOutcome {
                http_status,
                label: PostLabel::Posted,
            });
        }
        let label = if http_status == 429 || http_status >= 500 {
            PostLabel::Retry
        } else {
            PostLabel::Dropped
        };
        Err(CourierError::Application { http_status, label })
    }
}
