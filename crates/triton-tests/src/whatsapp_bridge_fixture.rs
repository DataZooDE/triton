//! In-repo fake WhatsApp Web bridge daemon for the bridge socket
//! adapter test (M-LIFECYCLE-1). No mocks per CLAUDE.md §1: a real
//! line-delimited JSON server on a real TCP listener, speaking the
//! bridge protocol the adapter expects:
//!
//!   daemon → triton: `{"type":"message","from":"<wa-id>","text":"…"}`
//!   triton → daemon: `{"type":"send","to":"<wa-id>","text":"…"}`
//!
//! On each connection the fixture emits one queued `message` line
//! (so the worker dispatches), captures every line the client writes
//! back (`send` replies), and supports a forced disconnect so the
//! adapter's bounded-reconnect lifecycle is observable. The
//! connection counter transitions 1 → 2 across a forced drop.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

struct FakeState {
    captured: Mutex<Vec<Value>>,
    connections: Mutex<u32>,
    active: AsyncMutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
    /// One inbound `message` payload emitted per connection (the last
    /// entry repeats for any further reconnects).
    messages: Vec<Value>,
}

pub struct FakeWhatsAppBridge {
    addr: SocketAddr,
    state: Arc<FakeState>,
    disconnect_tx: mpsc::UnboundedSender<()>,
}

impl FakeWhatsAppBridge {
    pub async fn start(messages: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(FakeState {
            captured: Mutex::new(Vec::new()),
            connections: Mutex::new(0),
            active: AsyncMutex::new(None),
            messages,
        });
        let (disconnect_tx, mut disconnect_rx) = mpsc::unbounded_channel::<()>();

        // Disconnect task: drops the active writer on command.
        {
            let state = state.clone();
            tokio::spawn(async move {
                while disconnect_rx.recv().await.is_some() {
                    let mut guard = state.active.lock().await;
                    if let Some(w) = guard.take() {
                        drop(w);
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
                    let (read, mut write) = stream.into_split();
                    let conn_index = {
                        let mut counter = state.connections.lock().unwrap();
                        *counter += 1;
                        *counter - 1 // 0-based
                    };
                    // Emit this connection's inbound message.
                    if !state.messages.is_empty() {
                        let idx = (conn_index as usize).min(state.messages.len() - 1);
                        let mut bytes =
                            serde_json::to_vec(&state.messages[idx]).unwrap_or_default();
                        bytes.push(b'\n');
                        let _ = write.write_all(&bytes).await;
                    }
                    {
                        let mut slot = state.active.lock().await;
                        *slot = Some(write);
                    }
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
                                        reader_state.captured.lock().unwrap().push(v);
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
            disconnect_tx,
        }
    }

    /// `tcp://127.0.0.1:<port>` for `TRITON_WHATSAPP_BRIDGE_ADDR`.
    pub fn tcp_uri(&self) -> String {
        format!("tcp://{}", self.addr)
    }

    pub fn connection_count(&self) -> u32 {
        *self.state.connections.lock().unwrap()
    }

    /// Lines the client (adapter) wrote back — the `send` replies.
    pub fn captured(&self) -> Vec<Value> {
        self.state.captured.lock().unwrap().clone()
    }

    pub fn force_disconnect(&self) {
        let _ = self.disconnect_tx.send(());
    }
}
