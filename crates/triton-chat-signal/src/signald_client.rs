//! Tiny signald connection manager.
//!
//! signald (https://signald.org/) listens on a Unix socket OR TCP
//! socket and speaks line-delimited JSON. Each line is one request
//! or one event. We hold ONE persistent connection per adapter and
//! reconnect with exponential backoff on failure (PR 34).
//!
//! Two halves:
//!   * `connect_loop` — runs forever (until shutdown). Connects,
//!     subscribes, reads events line-by-line and pushes each
//!     parsed `IncomingMessage` to the caller's handler. On any
//!     read/write error, sleeps with backoff and reconnects.
//!   * `Sender` — a clone-able handle that takes a JSON value and
//!     queues it for the write half. Guarded by an internal
//!     `tokio::sync::Mutex` so concurrent senders never interleave
//!     bytes inside a single line.
//!
//! The connection abstracts over TCP and Unix sockets via the
//! [`SignaldStream`] enum so tests can drive the adapter against a
//! local TCP listener and production can use the deploy's preferred
//! transport.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Initial reconnect delay (FR-L-2 graceful, NFR-O-1 backoff).
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
/// Maximum reconnect delay. Caps the exponential growth so a
/// long-running outage doesn't park us on a multi-minute backoff
/// after recovery.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Parsed signald inbound address.
///
/// Two URI schemes accepted:
///   * `tcp://host:port` — for substrate deploys where signald
///     runs on a sidecar reachable via the tailnet (the bot's
///     trust boundary is the network path; signald itself does
///     not enforce any message signature).
///   * `unix:///run/signald.sock` — for local-dev where signald
///     runs in the same alloc as triton.
#[derive(Debug, Clone)]
pub enum SignaldAddr {
    Tcp(String),
    Unix(std::path::PathBuf),
}

impl SignaldAddr {
    /// Parse a `tcp://host:port` or `unix:///path/to/socket` URI.
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
                "signald addr must start with `tcp://` or `unix://`: {s}"
            ))
        }
    }
}

/// Read or write half of a signald connection. The two halves are
/// split because the read loop and the write side need separate
/// ownership — but we keep them inside one enum so the surrounding
/// code can be transport-agnostic.
enum ReadHalf {
    Tcp(BufReader<tokio::net::tcp::OwnedReadHalf>),
    Unix(BufReader<tokio::net::unix::OwnedReadHalf>),
}

impl ReadHalf {
    async fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        match self {
            ReadHalf::Tcp(r) => r.read_line(buf).await,
            ReadHalf::Unix(r) => r.read_line(buf).await,
        }
    }
}

enum WriteHalf {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Unix(tokio::net::unix::OwnedWriteHalf),
}

impl WriteHalf {
    async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            WriteHalf::Tcp(w) => w.write_all(bytes).await,
            WriteHalf::Unix(w) => w.write_all(bytes).await,
        }
    }
    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            WriteHalf::Tcp(w) => w.shutdown().await,
            WriteHalf::Unix(w) => w.shutdown().await,
        }
    }
}

async fn connect(addr: &SignaldAddr) -> std::io::Result<(ReadHalf, WriteHalf)> {
    match addr {
        SignaldAddr::Tcp(addr) => {
            let stream = TcpStream::connect(addr).await?;
            let (r, w) = stream.into_split();
            Ok((ReadHalf::Tcp(BufReader::new(r)), WriteHalf::Tcp(w)))
        }
        SignaldAddr::Unix(path) => {
            let stream = UnixStream::connect(path).await?;
            let (r, w) = stream.into_split();
            Ok((ReadHalf::Unix(BufReader::new(r)), WriteHalf::Unix(w)))
        }
    }
}

/// A line-write handle to the currently-connected signald socket.
/// Cloning is cheap (it's an `Arc<Mutex<…>>` under the hood) so each
/// inbound handler can ship its reply without owning the writer.
///
/// The inner `Option` is `Some` while connected and `None` during a
/// reconnect window. Writes attempted during a reconnect window are
/// dropped with a warn log — the courier-side equivalent of a 5xx
/// post-back. Per FR-L-2 we never block a dispatch on the network.
#[derive(Clone)]
pub struct Sender {
    inner: Arc<Mutex<Option<WriteHalf>>>,
}

impl Sender {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Serialise `body` as one JSON line and write it. Returns
    /// `Err(SendError::Disconnected)` if there's no live connection;
    /// returns `Err(SendError::Io)` if the write itself failed (the
    /// caller's audit emitter should record this as a dropped post,
    /// not a retry-eligible one — the read loop is responsible for
    /// reconnecting).
    pub async fn send(&self, body: &Value) -> Result<(), SendError> {
        let mut bytes = serde_json::to_vec(body).map_err(SendError::Encode)?;
        // signald terminates requests with a newline.
        bytes.push(b'\n');
        let mut guard = self.inner.lock().await;
        match guard.as_mut() {
            Some(w) => w.write_all(&bytes).await.map_err(SendError::Io),
            None => Err(SendError::Disconnected),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("signald: not connected (reconnect in progress)")]
    Disconnected,
    #[error("signald: write failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("signald: serialise body: {0}")]
    Encode(#[source] serde_json::Error),
}

/// Events the connect loop forwards to the adapter. We forward raw
/// JSON values rather than typed events so the adapter can decide
/// per-message what shape to parse — keeps this client narrow.
#[derive(Debug)]
pub enum SignaldEvent {
    /// A fresh line arrived from signald. Parsing into a typed event
    /// (IncomingMessage, send_results, version, …) is the caller's
    /// responsibility.
    Line(Value),
    /// The connection was (re-)established. The adapter uses this
    /// signal to (re-)subscribe — subscriptions don't survive a
    /// reconnect on signald's side.
    Connected,
}

/// Run the signald connection loop. The returned `Sender` is live
/// the moment `Connected` is first emitted; before that, sends fail
/// with `Disconnected`. Cancelling `shutdown` ends the loop
/// gracefully and shuts down the write half (FR-L-2).
///
/// `events` carries `SignaldEvent::Connected` after every successful
/// (re-)connect, then one `SignaldEvent::Line(value)` per line read
/// until the next disconnect.
pub fn spawn_connect_loop(
    addr: SignaldAddr,
    events: mpsc::Sender<SignaldEvent>,
    shutdown: CancellationToken,
) -> (Sender, tokio::task::JoinHandle<()>) {
    let sender = Sender::new();
    let writer_slot = sender.inner.clone();
    let join = tokio::spawn(async move {
        let mut backoff = BACKOFF_INITIAL;
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            // Connect attempt.
            let conn = tokio::select! {
                r = connect(&addr) => r,
                _ = shutdown.cancelled() => break,
            };
            let (mut read, write) = match conn {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(
                        backoff_ms = backoff.as_millis() as u64,
                        "signald: connect failed: {e}",
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown.cancelled() => break,
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            // Install write half so `Sender::send` works.
            {
                let mut slot = writer_slot.lock().await;
                *slot = Some(write);
            }
            // Tell the adapter we're connected (it will re-subscribe).
            if events.send(SignaldEvent::Connected).await.is_err() {
                // Receiver dropped — adapter is gone; we should
                // shut down cleanly.
                break;
            }
            // Reset backoff once we've actually exchanged something
            // (we did: the OS handed us a stream). Doing it here
            // instead of after the first read keeps a flapping
            // connection from blowing the backoff ceiling on every
            // attempt.
            backoff = BACKOFF_INITIAL;

            // Read loop.
            loop {
                let mut line = String::new();
                let read_res = tokio::select! {
                    r = read.read_line(&mut line) => r,
                    _ = shutdown.cancelled() => {
                        // Graceful shutdown: drop the writer, then
                        // exit the outer loop on the next iteration's
                        // is_cancelled() check.
                        let mut slot = writer_slot.lock().await;
                        if let Some(mut w) = slot.take() {
                            let _ = w.shutdown().await;
                        }
                        return;
                    }
                };
                match read_res {
                    Ok(0) => {
                        // EOF — signald closed the connection.
                        tracing::warn!("signald: connection closed by peer; reconnecting");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(v) => {
                                if events.send(SignaldEvent::Line(v)).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                // Garbage line on the wire. signald
                                // ships well-formed JSON; a parse
                                // failure points at a protocol skew.
                                // Log + skip; don't tear down the
                                // socket on a single bad line.
                                tracing::warn!("signald: dropping unparseable line: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("signald: read error: {e}; reconnecting");
                        break;
                    }
                }
            }
            // Lost the connection. Drop the write half + back off.
            {
                let mut slot = writer_slot.lock().await;
                *slot = None;
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.cancelled() => break,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        // Shutdown path: drop the writer if we still own it.
        let mut slot = writer_slot.lock().await;
        if let Some(mut w) = slot.take() {
            let _ = w.shutdown().await;
        }
    });
    (sender, join)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tcp_addr() {
        let a = SignaldAddr::parse("tcp://signald.test:15432").unwrap();
        match a {
            SignaldAddr::Tcp(s) => assert_eq!(s, "signald.test:15432"),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn parse_unix_addr() {
        let a = SignaldAddr::parse("unix:///run/signald.sock").unwrap();
        match a {
            SignaldAddr::Unix(p) => assert_eq!(p, std::path::PathBuf::from("/run/signald.sock")),
            _ => panic!("expected unix"),
        }
    }

    #[test]
    fn parse_rejects_unknown_scheme() {
        assert!(SignaldAddr::parse("http://signald.test:15432").is_err());
        assert!(SignaldAddr::parse("signald.test:15432").is_err());
    }
}
