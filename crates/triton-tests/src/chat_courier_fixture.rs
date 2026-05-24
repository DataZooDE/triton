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

/// Response profile the fake should return on each `sendMessage`.
#[derive(Debug, Clone)]
pub enum Profile {
    /// Default — `{ok: true, result: {message_id: 1}}`.
    Ok,
    /// `{ok: false, error_code, description, parameters: {retry_after}}`.
    /// Use for testing Codex PR 18 blocker 2 — 200-with-ok:false.
    Application {
        error_code: i64,
        retry_after: Option<u64>,
    },
}

struct FakeState {
    captured: Mutex<Vec<SentMessage>>,
    profile: Profile,
}

pub struct FakeTelegramApi {
    addr: SocketAddr,
    state: Arc<FakeState>,
}

impl FakeTelegramApi {
    pub async fn start() -> Self {
        Self::with_profile(Profile::Ok).await
    }

    pub async fn with_profile(profile: Profile) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(FakeState {
            captured: Mutex::new(Vec::new()),
            profile,
        });

        let router = Router::new()
            .route("/bot{token}/sendMessage", post(handle_send_message))
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> Vec<SentMessage> {
        self.state.captured.lock().unwrap().clone()
    }
}

async fn handle_send_message(
    State(state): State<Arc<FakeState>>,
    Path(token): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state
        .captured
        .lock()
        .unwrap()
        .push(SentMessage { token, body });
    match &state.profile {
        Profile::Ok => Json(json!({ "ok": true, "result": { "message_id": 1 } })),
        Profile::Application {
            error_code,
            retry_after,
        } => {
            let mut params = serde_json::Map::new();
            if let Some(s) = retry_after {
                params.insert("retry_after".to_string(), json!(s));
            }
            Json(json!({
                "ok": false,
                "error_code": error_code,
                "description": "fake telegram application error",
                "parameters": params,
            }))
        }
    }
}
