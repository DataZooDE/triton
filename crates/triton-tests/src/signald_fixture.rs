//! In-repo fake signald daemon for the PR 34 Signal-adapter
//! integration tests.
//!
//! Speaks the actual line-delimited JSON wire shape on a real TCP
//! listener — no mocks per CLAUDE.md §1. The fixture accepts ONE
//! connection at a time (signald itself is single-connection per
//! adapter), captures every JSON line the client sends, and exposes
//! an event-emitter handle so the test can push `IncomingMessage`
//! envelopes back through the same socket.
//!
//! The fixture also lets a test trigger a forced disconnect so the
//! adapter's reconnect+resubscribe behaviour is observable.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

/// One captured line from the client (whatever the adapter wrote
/// into the socket). The fixture stores the parsed JSON so tests
/// can match on `type` + fields without re-parsing.
#[derive(Debug, Clone)]
pub struct CapturedLine {
    pub value: Value,
}

/// Handle a test holds onto for the lifetime of one fixture
/// instance. Cloning is cheap; the underlying state is `Arc`d.
#[derive(Clone)]
pub struct FakeSignald {
    addr: SocketAddr,
    state: Arc<FakeState>,
    /// Sender into the "emit this line to whoever's connected" channel.
    /// Lines are dropped silently when no client is connected (matches
    /// signald — events delivered between connections aren't replayed).
    emit_tx: mpsc::UnboundedSender<EmitCmd>,
}

struct FakeState {
    captured: Mutex<Vec<CapturedLine>>,
    /// Connection counter — bumped on every accept. The reconnect
    /// test asserts this transitions 1 → 2 after a forced disconnect.
    connections: Mutex<u32>,
    /// Pointer to the currently-active writer half, behind an
    /// async mutex so the emitter task can grab it without
    /// contending with future-accept paths.
    active: AsyncMutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
}

enum EmitCmd {
    Line(Value),
    /// Force-close the active connection so the adapter has to
    /// reconnect.
    Disconnect,
}

impl FakeSignald {
    /// Bind on `127.0.0.1:0`, start accepting connections in a
    /// background task, and return a handle.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(FakeState {
            captured: Mutex::new(Vec::new()),
            connections: Mutex::new(0),
            active: AsyncMutex::new(None),
        });
        let (emit_tx, mut emit_rx) = mpsc::unbounded_channel::<EmitCmd>();

        // Emitter task: pulls EmitCmds and writes them through the
        // currently-active writer half. Dropped when the fixture
        // is dropped (the sender is owned by the fixture).
        {
            let state = state.clone();
            tokio::spawn(async move {
                while let Some(cmd) = emit_rx.recv().await {
                    let mut guard = state.active.lock().await;
                    match cmd {
                        EmitCmd::Line(v) => {
                            if let Some(w) = guard.as_mut() {
                                let mut bytes = serde_json::to_vec(&v).unwrap_or_default();
                                bytes.push(b'\n');
                                let _ = w.write_all(&bytes).await;
                            }
                            // No active connection ⇒ drop silently.
                        }
                        EmitCmd::Disconnect => {
                            if let Some(w) = guard.take() {
                                drop(w);
                            }
                        }
                    }
                }
            });
        }

        // Accept loop.
        {
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _peer)) = listener.accept().await else {
                        return;
                    };
                    let (read, write) = stream.into_split();
                    {
                        let mut counter = state.connections.lock().unwrap();
                        *counter += 1;
                    }
                    {
                        let mut slot = state.active.lock().await;
                        *slot = Some(write);
                    }
                    // Per-connection reader task. Each line gets
                    // parsed and appended to `captured` for the test
                    // to inspect.
                    let reader_state = state.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(read);
                        loop {
                            let mut line = String::new();
                            match reader.read_line(&mut line).await {
                                Ok(0) => return,
                                Ok(_) => {
                                    let trimmed = line.trim();
                                    if trimmed.is_empty() {
                                        continue;
                                    }
                                    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                                        reader_state
                                            .captured
                                            .lock()
                                            .unwrap()
                                            .push(CapturedLine { value: v });
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                    });
                }
            });
        }

        Self {
            addr,
            state,
            emit_tx,
        }
    }

    /// `tcp://127.0.0.1:<port>` URI suitable for
    /// `TRITON_SIGNAL_SIGNALD_ADDR`.
    pub fn tcp_uri(&self) -> String {
        format!("tcp://{}", self.addr)
    }

    /// Number of times an accept has succeeded. Useful for the
    /// reconnect test (1 → 2 after a forced disconnect).
    pub fn connections(&self) -> u32 {
        *self.state.connections.lock().unwrap()
    }

    /// Snapshot of every line the client wrote so far.
    pub fn captured(&self) -> Vec<CapturedLine> {
        self.state.captured.lock().unwrap().clone()
    }

    /// Find the first captured line whose `type` field matches.
    pub fn first_with_type(&self, type_name: &str) -> Option<CapturedLine> {
        self.captured()
            .into_iter()
            .find(|c| c.value.get("type").and_then(|t| t.as_str()) == Some(type_name))
    }

    /// Count captured lines with the given `type`.
    pub fn count_with_type(&self, type_name: &str) -> usize {
        self.captured()
            .into_iter()
            .filter(|c| c.value.get("type").and_then(|t| t.as_str()) == Some(type_name))
            .count()
    }

    /// Push an arbitrary JSON envelope to the connected client.
    /// Dropped silently when no client is connected.
    pub fn emit(&self, v: Value) {
        let _ = self.emit_tx.send(EmitCmd::Line(v));
    }

    /// Construct + emit an `IncomingMessage` envelope shaped like
    /// the one signald sends after the adapter's `subscribe`.
    pub fn emit_incoming(&self, sender_uuid: &str, sender_number: Option<&str>, body: &str) {
        let mut source = serde_json::Map::new();
        source.insert("uuid".to_string(), Value::String(sender_uuid.to_string()));
        if let Some(n) = sender_number {
            source.insert("number".to_string(), Value::String(n.to_string()));
        }
        let envelope = serde_json::json!({
            "type": "IncomingMessage",
            "data": {
                "source": Value::Object(source),
                "data_message": {
                    "body": body,
                    "timestamp": 1_700_000_000_000u64,
                    "expires_in_seconds": 0,
                },
                "timestamp": 1_700_000_000_000u64,
            }
        });
        self.emit(envelope);
    }

    /// Force the active connection closed (drops the writer +
    /// signals EOF to the adapter). Reconnect test uses this.
    pub fn force_disconnect(&self) {
        let _ = self.emit_tx.send(EmitCmd::Disconnect);
    }

    /// Convenience: wait up to `deadline` for the captured lines to
    /// contain at least one entry of `type` == `type_name`.
    pub async fn wait_for_type(&self, type_name: &str, deadline: Duration) -> Option<CapturedLine> {
        let start = std::time::Instant::now();
        loop {
            if let Some(line) = self.first_with_type(type_name) {
                return Some(line);
            }
            if start.elapsed() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
