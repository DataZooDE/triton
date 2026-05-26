//! Discord Gateway socket inbound (FR-A-1.v0.2 `socket`,
//! M-LIFECYCLE-1).
//!
//! The Interactions adapter (`lib.rs`) is an HTTP webhook; the
//! Gateway adapter here is the persistent-WebSocket alternative. It
//! speaks the documented Gateway opcode protocol:
//!
//!   HELLO(10) → IDENTIFY(2) [or RESUME(6)] → READY/RESUMED(0) →
//!   DISPATCH(0) events; HEARTBEAT(1) every `heartbeat_interval`.
//!
//! On socket loss it reconnects with bounded exponential backoff and
//! RESUMEs (replaying from the last sequence) when it still holds a
//! session, re-IDENTIFYing otherwise — the M-LIFECYCLE-1 contract.
//! Auth is the bot token in IDENTIFY (no per-message signature), so
//! the manifest declares `signature: trusted_socket`.
//!
//! Dispatch + audit go through the same `Dispatcher` every adapter
//! uses; the reply is a REST `POST /channels/{id}/messages` carrying
//! the bot token (never logged — FR-AU-3).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use triton_core::{Dispatcher, PostOutcome, Principal, TritonError};
use triton_manifest::{Adapter, AdapterKind, IdentityKind, SignatureScheme};
use triton_secrets::SecretResolver;

use crate::{BuildError, PROTOCOL, SenderClaims};

/// Initial reconnect delay; doubles to the cap on repeated failure.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
/// Reconnect-delay ceiling — bounds the M-LIFECYCLE-1 clean-disconnect
/// recovery (≤ 30 s) so a long outage doesn't park on a huge backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Gateway intents: GUILD_MESSAGES (1<<9) | DIRECT_MESSAGES (1<<12) |
/// MESSAGE_CONTENT (1<<15). Enough to receive message text in guilds
/// and DMs.
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);

pub struct DiscordGatewayAdapter {
    name: String,
    bot_token: String,
    sender_table: HashMap<String, SenderClaims>,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
    gateway_url: String,
    rest_base: String,
    http: reqwest::Client,
}

impl DiscordGatewayAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        gateway_url: String,
        rest_base: String,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::Discord {
            return Err(BuildError::WrongKind);
        }
        if adapter.inbound.signature != SignatureScheme::TrustedSocket {
            return Err(BuildError::Unsupported(format!(
                "discord gateway (socket) adapter requires `signature: trusted_socket`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "discord gateway adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }

        let token_field = adapter
            .outbound
            .credentials
            .get("token")
            .ok_or(BuildError::MissingCredential("outbound.token"))?;
        let bot_token = resolver
            .resolve(token_field)
            .await
            .map_err(|e| BuildError::Resolve("outbound.token", e))?;
        if bot_token.trim().is_empty() {
            return Err(BuildError::Unsupported(
                "outbound.token must not be empty".into(),
            ));
        }

        let table_field = adapter
            .identity
            .credentials
            .get("table")
            .ok_or(BuildError::MissingCredential("identity.table"))?;
        let table_json = resolver
            .resolve(table_field)
            .await
            .map_err(|e| BuildError::Resolve("identity.table", e))?;
        let sender_table: HashMap<String, SenderClaims> =
            serde_json::from_str(&table_json).map_err(|e| BuildError::TableParse(e.to_string()))?;

        // FR-L-6: resolve correlation_key at boot so a bad ref fails
        // closed even though the gateway text path doesn't sign tokens.
        resolver
            .resolve(&adapter.correlation_key)
            .await
            .map_err(|e| BuildError::Resolve("correlation_key", e))?;

        const ADAPTER_HEADROOM: u32 = 10;
        let rate_limit = triton_core::ratelimit::TokenBucket::new(
            adapter
                .rate_limit
                .messages_per_sec
                .saturating_mul(ADAPTER_HEADROOM),
            adapter.rate_limit.burst.saturating_mul(ADAPTER_HEADROOM),
        );
        let per_tenant_limit = triton_core::ratelimit::PerTenantBuckets::new(
            adapter.rate_limit.messages_per_sec,
            adapter.rate_limit.burst,
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| BuildError::Unsupported(format!("gateway http client: {e}")))?;

        Ok(Self {
            name: name.to_string(),
            bot_token,
            sender_table,
            dispatcher,
            rate_limit,
            per_tenant_limit,
            gateway_url,
            rest_base,
            http,
        })
    }

    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut backoff = BACKOFF_INITIAL;
        // Held across reconnects to enable RESUME (Discord replays
        // events since `last_seq` from `session.resume_url`).
        let mut session: Option<Session> = None;
        let mut last_seq: i64 = -1;
        tracing::info!(adapter = %self.name, "discord gateway worker started");
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            // Resume against the session's resume URL when we hold one;
            // a fresh IDENTIFY uses the configured gateway URL.
            let base = session
                .as_ref()
                .map(|s| s.resume_url.clone())
                .unwrap_or_else(|| self.gateway_url.clone());
            let connect_url = with_query(&base);
            let connect = tokio_tungstenite::connect_async(&connect_url);
            let ws = tokio::select! {
                _ = shutdown.cancelled() => return,
                r = connect => match r {
                    Ok((ws, _resp)) => ws,
                    Err(e) => {
                        tracing::warn!(adapter = %self.name, error = %e, "gateway connect failed");
                        if backoff_sleep(&shutdown, backoff).await { return; }
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                },
            };
            // NB: backoff is reset INSIDE run_connection once HELLO is
            // received — a server that accepts then instantly closes
            // must still back off (Codex review), so connecting alone
            // does not count as progress.
            match self
                .run_connection(ws, &mut session, &mut last_seq, &mut backoff, &shutdown)
                .await
            {
                ConnOutcome::Shutdown => return,
                ConnOutcome::Reconnect => {}
                ConnOutcome::ReIdentify => {
                    session = None;
                    last_seq = -1;
                }
            }
            if backoff_sleep(&shutdown, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn run_connection(
        self: &Arc<Self>,
        ws: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        session: &mut Option<Session>,
        last_seq: &mut i64,
        backoff: &mut Duration,
        shutdown: &CancellationToken,
    ) -> ConnOutcome {
        let (write, mut read) = ws.split();
        let write = Arc::new(Mutex::new(write));
        let seq = Arc::new(AtomicI64::new(*last_seq));
        let awaiting_ack = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hb_cancel = CancellationToken::new();
        // Cancelled by the heartbeat task when the server misses an
        // ACK — turns a zombie connection into a reconnect.
        let conn_dead = CancellationToken::new();

        // First frame should be HELLO with the heartbeat interval.
        // Bounded by shutdown so a silent peer can't hold the drain.
        let heartbeat_ms = tokio::select! {
            _ = shutdown.cancelled() => return ConnOutcome::Shutdown,
            h = read_hello(&mut read) => match h {
                Some(ms) => ms,
                None => return ConnOutcome::Reconnect,
            },
        };
        // HELLO received → real progress; reset the backoff.
        *backoff = BACKOFF_INITIAL;

        // IDENTIFY or RESUME.
        let payload = match session.as_ref() {
            Some(s) => json!({
                "op": 6,
                "d": { "token": self.bot_token, "session_id": s.id, "seq": seq.load(Ordering::SeqCst) }
            }),
            None => json!({
                "op": 2,
                "d": {
                    "token": self.bot_token,
                    "intents": INTENTS,
                    "properties": { "os": "linux", "browser": "triton", "device": "triton" }
                }
            }),
        };
        if send_json(&write, &payload).await.is_err() {
            return ConnOutcome::Reconnect;
        }

        let hb = spawn_heartbeat(
            write.clone(),
            seq.clone(),
            awaiting_ack.clone(),
            heartbeat_ms,
            hb_cancel.clone(),
            conn_dead.clone(),
        );

        let outcome = loop {
            let next = tokio::select! {
                _ = shutdown.cancelled() => break ConnOutcome::Shutdown,
                // Heartbeat task signalled a missed ACK (zombie conn).
                _ = conn_dead.cancelled() => break ConnOutcome::Reconnect,
                m = read.next() => m,
            };
            let msg = match next {
                Some(Ok(m)) => m,
                _ => break ConnOutcome::Reconnect, // stream error
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                // A close frame's code decides resume vs re-identify.
                Message::Close(frame) => {
                    let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                    break if is_non_resumable_close(code) {
                        ConnOutcome::ReIdentify
                    } else {
                        ConnOutcome::Reconnect
                    };
                }
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                    continue;
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(s) = v.get("s").and_then(Value::as_i64) {
                seq.store(s, Ordering::SeqCst);
                *last_seq = s;
            }
            match v.get("op").and_then(Value::as_i64).unwrap_or(-1) {
                0 => {
                    let t = v.get("t").and_then(Value::as_str).unwrap_or("");
                    match t {
                        "READY" => {
                            // Store both the session id and the
                            // resume gateway URL Discord assigns for
                            // this session (fall back to the configured
                            // URL if absent).
                            if let Some(sid) = v.pointer("/d/session_id").and_then(Value::as_str) {
                                // We reconnect to this URL carrying the
                                // bot token in IDENTIFY/RESUME, so an
                                // untrusted `resume_gateway_url` is a
                                // credential-redirect risk. Only honour
                                // it when it's a Discord host (or shares
                                // the configured gateway's host, e.g. a
                                // self-hosted/test gateway); otherwise
                                // fall back to the configured URL.
                                let resume_url = v
                                    .pointer("/d/resume_gateway_url")
                                    .and_then(Value::as_str)
                                    .filter(|u| resume_url_is_trusted(u, &self.gateway_url))
                                    .unwrap_or(&self.gateway_url)
                                    .to_string();
                                *session = Some(Session {
                                    id: sid.to_string(),
                                    resume_url,
                                });
                            }
                        }
                        "MESSAGE_CREATE" => {
                            let d = v.get("d").cloned().unwrap_or(Value::Null);
                            self.handle_message_create(d).await;
                        }
                        _ => {}
                    }
                }
                // Server asked for an immediate heartbeat.
                1 => {
                    awaiting_ack.store(true, Ordering::SeqCst);
                    let _ = send_json(&write, &heartbeat_payload(&seq)).await;
                }
                // Heartbeat ACK clears the liveness flag.
                11 => awaiting_ack.store(false, Ordering::SeqCst),
                // RECONNECT: reconnect and RESUME.
                7 => break ConnOutcome::Reconnect,
                // INVALID_SESSION: `d: true` is resumable; `false`
                // (the default) means start fresh.
                9 => {
                    break if v.get("d").and_then(Value::as_bool) == Some(true) {
                        ConnOutcome::Reconnect
                    } else {
                        ConnOutcome::ReIdentify
                    };
                }
                _ => {}
            }
        };

        hb_cancel.cancel();
        hb.abort();
        outcome
    }

    async fn handle_message_create(self: &Arc<Self>, d: Value) {
        // Ignore bot authors (including our own echoes) and empty text.
        if d.pointer("/author/bot").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let author_id = d
            .pointer("/author/id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let content = d.get("content").and_then(Value::as_str).unwrap_or("");
        let channel_id = d.get("channel_id").and_then(Value::as_str).unwrap_or("");
        if author_id.is_empty() || content.is_empty() || channel_id.is_empty() {
            return;
        }

        // Adapter-wide rate limit (DoS floor), before sender lookup.
        if self.rate_limit.try_take().is_err() {
            self.dispatcher.record_rejection(
                &self.name,
                PROTOCOL,
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &TritonError::RateLimited(format!("discord gateway `{}` rate limit", self.name)),
            );
            return;
        }

        let Some(claims) = self.sender_table.get(author_id) else {
            self.dispatcher.record_rejection(
                &self.name,
                PROTOCOL,
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &TritonError::Auth(format!("unknown sender {author_id}")),
            );
            return;
        };

        if self.per_tenant_limit.try_take(&claims.tenant).is_err() {
            self.dispatcher.record_rejection(
                &self.name,
                PROTOCOL,
                &claims.sub,
                &claims.tenant,
                &uuid::Uuid::new_v4().to_string(),
                &TritonError::RateLimited(format!("tenant `{}` rate limit", claims.tenant)),
            );
            return;
        }

        let principal = Principal {
            sub: claims.sub.clone(),
            scopes: claims.scopes.clone(),
            tenant: claims.tenant.clone(),
            raw_token: String::new(),
            trace_id: uuid::Uuid::new_v4().to_string(),
        };
        let principal_for_post = principal.clone();
        let (tool, args) = route_command(content);

        let started = std::time::Instant::now();
        let result = self
            .dispatcher
            .invoke(tool, args, principal, PROTOCOL)
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(dispatch) => {
                let reply = reply_text(&dispatch.result);
                self.post_reply(channel_id, &reply, &principal_for_post, tool, latency_ms)
                    .await;
            }
            Err(e) => {
                tracing::warn!(adapter = %self.name, error = %e, "gateway dispatch failed");
            }
        }
    }

    async fn post_reply(
        self: &Arc<Self>,
        channel_id: &str,
        content: &str,
        principal: &Principal,
        tool: &str,
        dispatch_latency_ms: u64,
    ) {
        let url = format!(
            "{}/api/v10/channels/{}/messages",
            self.rest_base, channel_id
        );
        let started = std::time::Instant::now();
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&json!({ "content": content }))
            .send()
            .await;
        let latency = dispatch_latency_ms + started.elapsed().as_millis() as u64;
        match resp {
            Ok(r) if r.status().is_success() => {
                self.dispatcher.record_post(
                    tool,
                    PROTOCOL,
                    principal,
                    latency,
                    Ok((r.status().as_u16(), PostOutcome::Posted, None)),
                );
            }
            Ok(r) => {
                let status = r.status().as_u16();
                let label = if status >= 500 || status == 429 {
                    PostOutcome::Retry
                } else {
                    PostOutcome::Dropped
                };
                // Don't log the URL (it's channel-scoped, not secret,
                // but keep the token-free discipline uniform).
                let provider = TritonError::Provider(format!("discord messages POST {status}"));
                self.dispatcher.record_post(
                    tool,
                    PROTOCOL,
                    principal,
                    latency,
                    Err((&provider, status, label, None)),
                );
            }
            Err(_) => {
                let provider = TritonError::Provider("discord messages POST transport".into());
                self.dispatcher.record_post(
                    tool,
                    PROTOCOL,
                    principal,
                    latency,
                    Err((&provider, 0, PostOutcome::Retry, None)),
                );
            }
        }
    }
}

enum ConnOutcome {
    Shutdown,
    Reconnect,
    ReIdentify,
}

/// A resumable Gateway session: the id to RESUME with and the
/// session-specific gateway URL Discord assigned in READY.
struct Session {
    id: String,
    resume_url: String,
}

/// Gateway close codes that are NOT resumable — the client must start
/// a fresh session (re-IDENTIFY) rather than RESUME. Per Discord's
/// documented close-code table: authentication failed (4004), invalid
/// shard (4010), sharding required (4011), invalid API version (4012),
/// invalid intents (4013), disallowed intents (4014).
fn is_non_resumable_close(code: u16) -> bool {
    matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014)
}

/// Extract the lowercased host from a `ws://`/`wss://` URL via a real
/// URL parser (handles bracketed IPv6, userinfo, case, ports). `None`
/// if the scheme isn't ws(s) or there's no host. (Codex review:
/// hand-rolled splitting mis-parsed IPv6 and was case-sensitive.)
fn ws_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "ws" | "wss") {
        return None;
    }
    parsed
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

/// True if `resume_url` is safe to reconnect to with the bot token.
/// Trusted when it's a Discord host (`discord.gg` / `discord.com` or a
/// subdomain) or shares the host of the configured `gateway_url`
/// (covers a self-hosted / test gateway). Anything else is refused so
/// a malicious `resume_gateway_url` can't redirect our credentials to
/// an attacker — Codex security review.
fn resume_url_is_trusted(resume_url: &str, gateway_url: &str) -> bool {
    let Some(host) = ws_host(resume_url) else {
        return false;
    };
    let is_discord = host == "discord.gg"
        || host == "discord.com"
        || host.ends_with(".discord.gg")
        || host.ends_with(".discord.com");
    is_discord || ws_host(gateway_url).is_some_and(|g| g == host)
}

/// Append the Gateway query Discord requires. `base` is either the
/// configured gateway URL or a session's resume URL.
fn with_query(base: &str) -> String {
    if base.contains('?') {
        base.to_string()
    } else {
        format!("{base}?v=10&encoding=json")
    }
}

fn heartbeat_payload(seq: &AtomicI64) -> Value {
    let s = seq.load(Ordering::SeqCst);
    if s < 0 {
        json!({ "op": 1, "d": Value::Null })
    } else {
        json!({ "op": 1, "d": s })
    }
}

type WsWrite = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsRead = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn read_hello(read: &mut WsRead) -> Option<u64> {
    while let Some(Ok(msg)) = read.next().await {
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).ok()?;
            if v.get("op").and_then(Value::as_i64) == Some(10) {
                return v.pointer("/d/heartbeat_interval").and_then(Value::as_u64);
            }
        }
    }
    None
}

async fn send_json(write: &Arc<Mutex<WsWrite>>, v: &Value) -> Result<(), ()> {
    let mut w = write.lock().await;
    w.send(Message::Text(v.to_string())).await.map_err(|_| ())
}

fn spawn_heartbeat(
    write: Arc<Mutex<WsWrite>>,
    seq: Arc<AtomicI64>,
    awaiting_ack: Arc<std::sync::atomic::AtomicBool>,
    interval_ms: u64,
    cancel: CancellationToken,
    conn_dead: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_millis(interval_ms.max(1));
        // Discord recommends jittering the FIRST heartbeat by a random
        // fraction of the interval to avoid a thundering herd. A
        // coarse deterministic jitter (half the interval) is enough
        // for a single instance and needs no RNG dependency.
        let first = interval / 2;
        let mut next = first;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(next) => {}
            }
            next = interval;
            // If the previous heartbeat was never ACKed, the
            // connection is a zombie (Discord: terminate + reconnect).
            if awaiting_ack.load(Ordering::SeqCst) {
                conn_dead.cancel();
                return;
            }
            awaiting_ack.store(true, Ordering::SeqCst);
            if send_json(&write, &heartbeat_payload(&seq)).await.is_err() {
                conn_dead.cancel();
                return;
            }
        }
    })
}

async fn backoff_sleep(shutdown: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

/// Minimal command router (mirrors the other adapters): `/narrate x`
/// → narrate, else echo the whole message.
fn route_command(text: &str) -> (&'static str, Value) {
    if let Some(rest) = text.strip_prefix("/narrate ") {
        return ("narrate", json!({ "subject": rest }));
    }
    if text == "/narrate" {
        return ("narrate", json!({ "subject": "" }));
    }
    ("echo", json!({ "message": text }))
}

/// Best-effort plain-text reply from a tool result: echo's `{echo}`,
/// an A2UI surface's text/narration components, else the JSON.
fn reply_text(result: &Value) -> String {
    if let Some(s) = result.get("echo").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(components) = result
        .pointer("/surface/components")
        .and_then(Value::as_array)
    {
        let mut parts = Vec::new();
        for c in components {
            match c.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(s) = c.get("value").and_then(Value::as_str) {
                        parts.push(s.to_string());
                    }
                }
                Some("narration") => {
                    if let Some(s) = c.get("text").and_then(Value::as_str) {
                        parts.push(s.to_string());
                    }
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    serde_json::to_string(result).unwrap_or_default()
}

#[cfg(test)]
mod resume_tests {
    use super::resume_url_is_trusted;

    #[test]
    fn trusts_discord_hosts_and_configured_host() {
        let configured = "wss://gateway.discord.gg";
        assert!(resume_url_is_trusted(
            "wss://us-east1.discord.gg",
            configured
        ));
        assert!(resume_url_is_trusted(
            "wss://gateway.discord.gg",
            configured
        ));
        // Case-insensitive scheme + host (Codex LOW).
        assert!(resume_url_is_trusted(
            "WSS://Gateway.Discord.GG",
            configured
        ));
        // Self-hosted / test gateway: same host as configured.
        assert!(resume_url_is_trusted(
            "ws://127.0.0.1:9443",
            "ws://127.0.0.1:8443"
        ));
    }

    #[test]
    fn refuses_foreign_and_malformed_resume_urls() {
        let configured = "wss://gateway.discord.gg";
        assert!(!resume_url_is_trusted("wss://evil.example", configured));
        // Lookalike that merely contains the string is not a suffix match.
        assert!(!resume_url_is_trusted(
            "wss://discord.gg.evil.com",
            configured
        ));
        assert!(!resume_url_is_trusted("https://discord.gg", configured)); // wrong scheme
        assert!(!resume_url_is_trusted("garbage", configured));
    }
}
