//! Fake `api.telegram.org` for the PR 18 outbound courier.
//!
//! The Telegram Bot API base is `https://api.telegram.org`; methods
//! sit under `/bot{token}/{method}` (the token is part of the path,
//! not a header). This fixture stands up a tiny axum server that
//! captures every `sendMessage` body it receives so a test can
//! assert on `chat_id` / `text` after the binary's courier fires.
//!
//! No mocks per CLAUDE.md §1: this is a real HTTP server speaking
//! the Telegram Bot API wire shape over real TCP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// One captured `sendMessage` invocation. The token in the URL
/// path is asserted on so tests can confirm the adapter actually
/// used the resolved bot token (and not, e.g., a literal manifest
/// placeholder that survived a misconfigured Vault wiring).
#[derive(Debug, Clone)]
pub struct SentMessage {
    pub token: String,
    pub body: Value,
}

pub struct FakeTelegramApi {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<SentMessage>>>,
}

impl FakeTelegramApi {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<SentMessage>>> = Arc::new(Mutex::new(Vec::new()));

        let router = Router::new()
            .route("/bot{token}/sendMessage", post(handle_send_message))
            .with_state(captured.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, captured }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> Vec<SentMessage> {
        self.captured.lock().unwrap().clone()
    }
}

async fn handle_send_message(
    State(captured): State<Arc<Mutex<Vec<SentMessage>>>>,
    Path(token): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    captured.lock().unwrap().push(SentMessage { token, body });
    // Telegram Bot API success envelope.
    Json(json!({ "ok": true, "result": { "message_id": 1 } }))
}
