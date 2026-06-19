//! WhatsApp Web bridge socket inbound (FR-A-1.v0.2 `socket`,
//! M-LIFECYCLE-1).
//!
//! The Cloud-API adapter (`lib.rs`) is an HTTP webhook; this is the
//! persistent-socket alternative for the canonical WhatsApp **Web**
//! transport. Like Signal/signald, Triton does NOT implement the
//! WhatsApp protocol itself — it connects to a local **bridge daemon**
//! (a Baileys-style sidecar that terminates the WhatsApp Web session
//! inside the trust boundary) over a line-delimited JSON socket:
//!
//!   inbound  (daemon → triton): `{"type":"message","from":"<wa-id>","text":"…"}`
//!   reply    (triton → daemon): `{"type":"send","to":"<wa-id>","text":"…"}`
//!
//! The session, QR re-pairing, and end-to-end crypto live in the
//! bridge; Triton just reconnects to the bridge socket with bounded
//! exponential backoff (M-LIFECYCLE-1) and feeds inbound messages
//! through the shared dispatcher. Auth is the session-locality of the
//! bridge (NFR-S-4 / the C-11 model), so the manifest declares
//! `signature: trusted_socket`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::sync::CancellationToken;
use triton_core::{Dispatcher, PostOutcome, Principal, TritonError};
use triton_manifest::{
    Adapter, AdapterKind, IdentityKind, InboundKind, OutboundKind, SignatureScheme,
};
use triton_secrets::SecretResolver;

use crate::{BuildError, PROTOCOL, SenderClaims};

const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Parsed bridge socket address. Accepts `tcp://host:port` (the bridge
/// reachable over loopback / the tailnet) and `unix:///path` (a
/// co-located daemon socket) — same two schemes as signald.
#[derive(Debug, Clone)]
pub enum BridgeAddr {
    Tcp(String),
    Unix(std::path::PathBuf),
}

impl BridgeAddr {
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("tcp://") {
            if rest.is_empty() {
                return Err("tcp:// requires host:port".into());
            }
            Ok(Self::Tcp(rest.to_string()))
        } else if let Some(rest) = s.strip_prefix("unix://") {
            if rest.is_empty() {
                return Err("unix:// requires a path".into());
            }
            Ok(Self::Unix(std::path::PathBuf::from(rest)))
        } else {
            Err(format!(
                "whatsapp bridge addr must start with `tcp://` or `unix://`: {s}"
            ))
        }
    }
}

pub struct WhatsAppBridgeAdapter {
    name: String,
    bridge_addr: BridgeAddr,
    sender_table: HashMap<String, SenderClaims>,
    /// Manifest `tool`: where plain inbound text dispatches (default
    /// `echo`). Commands (`/narrate` etc.) keep their special routes.
    inbound_tool: String,
    dispatcher: Arc<Dispatcher>,
    rate_limit: triton_core::ratelimit::TokenBucket,
    per_tenant_limit: triton_core::ratelimit::PerTenantBuckets,
}

impl WhatsAppBridgeAdapter {
    pub async fn from_manifest(
        name: &str,
        adapter: &Adapter,
        resolver: &dyn SecretResolver,
        dispatcher: Arc<Dispatcher>,
        bridge_addr: &str,
    ) -> Result<Self, BuildError> {
        if adapter.kind != AdapterKind::WhatsappWeb {
            return Err(BuildError::WrongKind);
        }
        // The bridge is socket-in / socket-out by construction. Reject
        // a mixed shape (e.g. inbound.kind=socket + outbound.kind=
        // rest_api) rather than silently ignore the declared REST
        // outbound (Codex review).
        if adapter.inbound.kind != InboundKind::Socket {
            return Err(BuildError::Unsupported(format!(
                "whatsapp bridge adapter requires `inbound.kind: socket`; got {:?}",
                adapter.inbound.kind
            )));
        }
        if adapter.outbound.kind != OutboundKind::Socket {
            return Err(BuildError::Unsupported(format!(
                "whatsapp bridge adapter requires `outbound.kind: socket`; got {:?}",
                adapter.outbound.kind
            )));
        }
        if adapter.inbound.signature != SignatureScheme::TrustedSocket {
            return Err(BuildError::Unsupported(format!(
                "whatsapp bridge (socket) adapter requires `signature: trusted_socket`; got {:?}",
                adapter.inbound.signature
            )));
        }
        if adapter.identity.kind != IdentityKind::SenderTable {
            return Err(BuildError::Unsupported(format!(
                "whatsapp bridge adapter requires `identity.kind: sender_table`; got {:?}",
                adapter.identity.kind
            )));
        }
        let bridge_addr = BridgeAddr::parse(bridge_addr).map_err(BuildError::Unsupported)?;

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
        // closed even though the bridge text path doesn't sign tokens.
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

        Ok(Self {
            name: name.to_string(),
            bridge_addr,
            sender_table,
            inbound_tool: adapter.tool.clone(),
            dispatcher,
            rate_limit,
            per_tenant_limit,
        })
    }

    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut backoff = BACKOFF_INITIAL;
        tracing::info!(adapter = %self.name, "whatsapp bridge worker started");
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let conn = tokio::select! {
                _ = shutdown.cancelled() => return,
                r = connect(&self.bridge_addr) => r,
            };
            let stream = match conn {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(adapter = %self.name, error = %e, "whatsapp bridge connect failed");
                    if backoff_sleep(&shutdown, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            // Connected → real progress; reset the backoff.
            backoff = BACKOFF_INITIAL;
            if self.read_loop(stream, &shutdown).await {
                return; // shutdown observed mid-connection
            }
            if backoff_sleep(&shutdown, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Read line-delimited JSON until EOF / error / shutdown. Returns
    /// `true` if shutdown was observed (caller should exit), `false`
    /// to reconnect.
    async fn read_loop(
        self: &Arc<Self>,
        stream: BridgeStream,
        shutdown: &CancellationToken,
    ) -> bool {
        let (read, mut write) = stream.split();
        let mut reader = BufReader::new(read);
        loop {
            let mut line = String::new();
            let n = tokio::select! {
                _ = shutdown.cancelled() => return true,
                r = reader.read_line(&mut line) => r,
            };
            match n {
                Ok(0) => {
                    tracing::warn!(adapter = %self.name, "whatsapp bridge closed by peer; reconnecting");
                    return false;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                        tracing::warn!(adapter = %self.name, "whatsapp bridge: dropping unparseable line");
                        continue;
                    };
                    if v.get("type").and_then(Value::as_str) == Some("message") {
                        self.handle_message(&v, &mut write).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(adapter = %self.name, error = %e, "whatsapp bridge read error; reconnecting");
                    return false;
                }
            }
        }
    }

    async fn handle_message(self: &Arc<Self>, v: &Value, write: &mut WriteHalf) {
        let from = v.get("from").and_then(Value::as_str).unwrap_or("");
        let text = v.get("text").and_then(Value::as_str).unwrap_or("");
        if from.is_empty() || text.is_empty() {
            return;
        }
        if self.rate_limit.try_take().is_err() {
            self.dispatcher.record_rejection(
                &self.name,
                PROTOCOL,
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &TritonError::RateLimited(format!("whatsapp bridge `{}` rate limit", self.name)),
            );
            return;
        }
        let Some(claims) = self.sender_table.get(from) else {
            self.dispatcher.record_rejection(
                &self.name,
                PROTOCOL,
                "-",
                "-",
                &uuid::Uuid::new_v4().to_string(),
                &TritonError::Auth(format!("unknown sender {from}")),
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
            groups: claims.groups.clone(),
            tenant: claims.tenant.clone(),
            raw_token: String::new(),
            trace_id: uuid::Uuid::new_v4().to_string(),
        };
        let principal_for_post = principal.clone();
        let (tool, args) = route_command(text, &self.inbound_tool);

        let started = std::time::Instant::now();
        let result = self
            .dispatcher
            .invoke(&tool, args, principal, PROTOCOL)
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(dispatch) => {
                let reply = reply_text(&dispatch.result);
                let body = json!({ "type": "send", "to": from, "text": reply });
                match write_line(write, &body).await {
                    Ok(()) => self.dispatcher.record_post(
                        &tool,
                        PROTOCOL,
                        &principal_for_post,
                        latency_ms,
                        Ok((200, PostOutcome::Posted, None)),
                    ),
                    Err(_) => {
                        let provider = TritonError::Provider("whatsapp bridge write failed".into());
                        self.dispatcher.record_post(
                            &tool,
                            PROTOCOL,
                            &principal_for_post,
                            latency_ms,
                            Err((&provider, 0, PostOutcome::Retry, None)),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(adapter = %self.name, error = %e, "whatsapp bridge dispatch failed");
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

enum BridgeStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

enum ReadHalf {
    Tcp(tokio::net::tcp::OwnedReadHalf),
    Unix(tokio::net::unix::OwnedReadHalf),
}
enum WriteHalf {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Unix(tokio::net::unix::OwnedWriteHalf),
}

impl BridgeStream {
    fn split(self) -> (ReadHalf, WriteHalf) {
        match self {
            BridgeStream::Tcp(s) => {
                let (r, w) = s.into_split();
                (ReadHalf::Tcp(r), WriteHalf::Tcp(w))
            }
            BridgeStream::Unix(s) => {
                let (r, w) = s.into_split();
                (ReadHalf::Unix(r), WriteHalf::Unix(w))
            }
        }
    }
}

impl tokio::io::AsyncRead for ReadHalf {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ReadHalf::Tcp(r) => std::pin::Pin::new(r).poll_read(cx, buf),
            ReadHalf::Unix(r) => std::pin::Pin::new(r).poll_read(cx, buf),
        }
    }
}

async fn write_line(write: &mut WriteHalf, body: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(body).unwrap_or_default();
    bytes.push(b'\n');
    match write {
        WriteHalf::Tcp(w) => w.write_all(&bytes).await,
        WriteHalf::Unix(w) => w.write_all(&bytes).await,
    }
}

async fn connect(addr: &BridgeAddr) -> Result<BridgeStream, ConnError> {
    match addr {
        BridgeAddr::Tcp(a) => Ok(BridgeStream::Tcp(TcpStream::connect(a).await?)),
        BridgeAddr::Unix(p) => Ok(BridgeStream::Unix(UnixStream::connect(p).await?)),
    }
}

async fn backoff_sleep(shutdown: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

fn route_command(text: &str, default_tool: &str) -> (String, Value) {
    if let Some(rest) = text.strip_prefix("/narrate ") {
        return ("narrate".to_string(), json!({ "subject": rest }));
    }
    if text == "/narrate" {
        return ("narrate".to_string(), json!({ "subject": "" }));
    }
    (default_tool.to_string(), json!({ "message": text }))
}

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
mod tests {
    use super::*;

    #[test]
    fn parse_bridge_addr_schemes() {
        assert!(matches!(
            BridgeAddr::parse("tcp://127.0.0.1:9000").unwrap(),
            BridgeAddr::Tcp(_)
        ));
        assert!(matches!(
            BridgeAddr::parse("unix:///run/wa.sock").unwrap(),
            BridgeAddr::Unix(_)
        ));
        assert!(BridgeAddr::parse("http://x").is_err());
        assert!(BridgeAddr::parse("tcp://").is_err());
    }

    #[test]
    fn route_command_narrate_and_echo() {
        assert_eq!(route_command("/narrate hi", "echo").0, "narrate");
        assert_eq!(route_command("hello", "echo").0, "echo");
        // The plain-text fallback honours the configured tool.
        assert_eq!(route_command("hello", "assistant").0, "assistant");
    }
}
