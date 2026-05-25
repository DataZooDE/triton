//! Fake Discord Gateway + minimal REST, for the gateway socket
//! inbound test (M-LIFECYCLE-1). No mocks per CLAUDE.md §1: a real
//! WebSocket server (axum `ws`) speaking the Discord Gateway opcode
//! shape over real TCP, plus a real HTTP route capturing the bot's
//! REST reply.
//!
//! Gateway flow served:
//!   1. on connect → send HELLO (op 10) with a heartbeat_interval;
//!   2. accept IDENTIFY (op 2) or RESUME (op 6);
//!   3. send READY (op 0, t=READY) / RESUMED (op 0, t=RESUMED);
//!   4. send one MESSAGE_CREATE (op 0, t=MESSAGE_CREATE) per
//!      connection so the test can observe dispatch on first connect
//!      AND again after a forced reconnect;
//!   5. ignore inbound heartbeats (op 1).
//!
//! `force_disconnect()` drops the active socket so the adapter's
//! bounded-reconnect lifecycle is observable; `connection_count()`
//! transitions 1 → 2 across the drop. `captured_replies()` returns
//! the REST messages the bot posted back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, mpsc};

struct GwState {
    connections: AtomicU64,
    /// Notified to force-drop the currently-connected socket.
    disconnect: Notify,
    /// Each connection sends one of these channel_id/content/author
    /// MESSAGE_CREATE payloads (round-robins; clamps at last).
    messages: Vec<Value>,
    captured_replies: Mutex<Vec<CapturedReply>>,
    /// The bot token the gateway saw in the most recent IDENTIFY.
    identify_tokens: Mutex<Vec<String>>,
    /// One entry per connection's first auth op: "identify" (op 2) or
    /// "resume" (op 6). Lets the reconnect test prove the client
    /// actually RESUMEd rather than re-IDENTIFYing.
    conn_modes: Mutex<Vec<String>>,
    /// The session id the most recent RESUME (op 6) presented.
    resumed_session_ids: Mutex<Vec<String>>,
    /// This server's own gateway URL, handed back as
    /// `resume_gateway_url` in READY so the client resumes against us.
    self_ws_url: std::sync::Mutex<String>,
    /// heartbeat_interval advertised in HELLO (ms).
    heartbeat_ms: u64,
    /// When false, the server never ACKs (op 11) the client's
    /// heartbeats — used to test the zombie-connection detection that
    /// must trigger a reconnect with no forced disconnect.
    ack_heartbeats: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedReply {
    pub channel_id: String,
    pub body: Value,
    pub authorization: String,
}

pub struct FakeDiscordGateway {
    addr: SocketAddr,
    state: Arc<GwState>,
}

impl FakeDiscordGateway {
    /// Start a gateway that emits one MESSAGE_CREATE per connection.
    /// `messages[i]` is sent on the i-th connection (the last entry
    /// repeats for any further reconnects). Heartbeats are ACKed
    /// normally.
    pub async fn start(messages: Vec<Value>) -> Self {
        Self::build(messages, 45_000, true).await
    }

    /// Like [`Self::start`] but advertises a short heartbeat interval
    /// and NEVER ACKs heartbeats — so the client must detect the
    /// zombie connection and reconnect on its own (no forced drop).
    pub async fn start_zombie(messages: Vec<Value>) -> Self {
        Self::build(messages, 200, false).await
    }

    async fn build(messages: Vec<Value>, heartbeat_ms: u64, ack_heartbeats: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(GwState {
            connections: AtomicU64::new(0),
            disconnect: Notify::new(),
            messages,
            captured_replies: Mutex::new(Vec::new()),
            identify_tokens: Mutex::new(Vec::new()),
            conn_modes: Mutex::new(Vec::new()),
            resumed_session_ids: Mutex::new(Vec::new()),
            self_ws_url: std::sync::Mutex::new(format!("ws://{addr}/gateway")),
            heartbeat_ms,
            ack_heartbeats,
        });
        let app = Router::new()
            .route("/gateway", get(ws_upgrade))
            .route(
                "/api/v10/channels/{channel_id}/messages",
                post(capture_reply),
            )
            .with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, state }
    }

    /// `ws://127.0.0.1:<port>/gateway` — the gateway URL the adapter
    /// connects to (the `?v=&encoding=` query the client appends is
    /// ignored by the fixture).
    pub fn gateway_url(&self) -> String {
        format!("ws://{}/gateway", self.addr)
    }

    /// `http://127.0.0.1:<port>` — the REST base for reply capture.
    pub fn rest_base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn connection_count(&self) -> u64 {
        self.state.connections.load(Ordering::SeqCst)
    }

    pub fn force_disconnect(&self) {
        self.state.disconnect.notify_waiters();
    }

    pub async fn captured_replies(&self) -> Vec<CapturedReply> {
        self.state.captured_replies.lock().await.clone()
    }

    pub async fn identify_tokens(&self) -> Vec<String> {
        self.state.identify_tokens.lock().await.clone()
    }

    /// One entry per connection: "identify" or "resume".
    pub async fn connection_modes(&self) -> Vec<String> {
        self.state.conn_modes.lock().await.clone()
    }

    /// Session ids presented in RESUME (op 6) payloads.
    pub async fn resumed_session_ids(&self) -> Vec<String> {
        self.state.resumed_session_ids.lock().await.clone()
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<GwState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| gateway_conn(socket, state))
}

async fn gateway_conn(mut socket: WebSocket, state: Arc<GwState>) {
    let conn_index = state.connections.fetch_add(1, Ordering::SeqCst); // 0-based
    let heartbeat_interval = state.heartbeat_ms;

    // 1. HELLO.
    if socket
        .send(Message::Text(
            json!({ "op": 10, "d": { "heartbeat_interval": heartbeat_interval } })
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    // 2. Drive the connection: read frames, answer IDENTIFY/RESUME
    //    with READY/RESUMED + the per-connection MESSAGE_CREATE, and
    //    bail when force_disconnect fires.
    let (tx, mut rx) = mpsc::channel::<Message>(8);
    loop {
        tokio::select! {
            _ = state.disconnect.notified() => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            outgoing = rx.recv() => {
                if let Some(m) = outgoing
                    && socket.send(m).await.is_err()
                {
                    return;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { return; };
                let Message::Text(txt) = msg else { continue; };
                let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue; };
                let op = v.get("op").and_then(Value::as_i64).unwrap_or(-1);
                match op {
                    // IDENTIFY → record token, READY (with a real
                    // resume_gateway_url pointing back at us), then a
                    // message.
                    2 => {
                        state.conn_modes.lock().await.push("identify".to_string());
                        if let Some(tok) = v.pointer("/d/token").and_then(Value::as_str) {
                            state.identify_tokens.lock().await.push(tok.to_string());
                        }
                        let resume_url = state.self_ws_url.lock().unwrap().clone();
                        let ready = json!({
                            "op": 0, "s": 1, "t": "READY",
                            "d": { "session_id": "sess-abc", "resume_gateway_url": resume_url }
                        });
                        let _ = tx.send(Message::Text(ready.to_string().into())).await;
                        send_message_create(&state, &tx, conn_index).await;
                    }
                    // RESUME → record the presented session id, RESUMED,
                    // then a message (so the test sees a dispatch after
                    // reconnect too).
                    6 => {
                        state.conn_modes.lock().await.push("resume".to_string());
                        if let Some(sid) = v.pointer("/d/session_id").and_then(Value::as_str) {
                            state.resumed_session_ids.lock().await.push(sid.to_string());
                        }
                        let resumed = json!({ "op": 0, "s": 2, "t": "RESUMED", "d": {} });
                        let _ = tx.send(Message::Text(resumed.to_string().into())).await;
                        send_message_create(&state, &tx, conn_index).await;
                    }
                    // Heartbeat (op 1) → ACK (op 11). The zombie
                    // fixture (ack_heartbeats=false) deliberately drops
                    // through and never ACKs.
                    1 if state.ack_heartbeats => {
                        let _ = tx
                            .send(Message::Text(json!({ "op": 11 }).to_string().into()))
                            .await;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn send_message_create(state: &Arc<GwState>, tx: &mpsc::Sender<Message>, conn_index: u64) {
    if state.messages.is_empty() {
        return;
    }
    let idx = (conn_index as usize).min(state.messages.len() - 1);
    let d = state.messages[idx].clone();
    let evt = json!({ "op": 0, "s": 3, "t": "MESSAGE_CREATE", "d": d });
    let _ = tx.send(Message::Text(evt.to_string().into())).await;
}

async fn capture_reply(
    State(state): State<Arc<GwState>>,
    Path(channel_id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.captured_replies.lock().await.push(CapturedReply {
        channel_id,
        body,
        authorization,
    });
    axum::Json(json!({ "id": "msg-1", "content": "ok" }))
}
