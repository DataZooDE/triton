//! Fake `api.telegram.org` for the PR 18 outbound courier.
//!
//! The Telegram Bot API base is `https://api.telegram.org`; methods
//! sit under `/bot{token}/{method}` (the token is part of the path,
//! not a header). This fixture stands up a tiny axum server that
//! captures every `sendMessage` body it receives so a test can
//! assert on `chat_id` / `text` after the binary's courier fires.
//!
//! PR 31 adds `FakeWhatsAppApi` — same shape, but speaks the Meta
//! Graph API wire format: `POST /v18.0/{phone_number_id}/messages`
//! with the bearer token as a header (not URL-embedded) and a
//! `{messaging_product, to, type, text}` body.
//!
//! No mocks per CLAUDE.md §1: real HTTP server over real TCP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
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

// ---------- WhatsApp Cloud API fake (PR 31) ----------

/// One captured `messages` POST against the fake WhatsApp Cloud
/// API. `phone_number_id` is the URL-path segment; `authorization`
/// is the verbatim `Authorization` header value so tests can assert
/// the bearer token actually made it through credential resolution.
#[derive(Debug, Clone)]
pub struct WhatsAppSentMessage {
    pub phone_number_id: String,
    pub authorization: String,
    pub body: Value,
}

struct WhatsAppState {
    captured: Mutex<Vec<WhatsAppSentMessage>>,
}

/// Fake `graph.facebook.com` for the PR 31 outbound courier. Speaks
/// the `/v18.0/{phone_number_id}/messages` wire shape with a stub
/// `{messaging_product, contacts, messages: [{id: "wamid.stub"}]}`
/// response on every POST.
pub struct FakeWhatsAppApi {
    addr: SocketAddr,
    state: Arc<WhatsAppState>,
}

impl FakeWhatsAppApi {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(WhatsAppState {
            captured: Mutex::new(Vec::new()),
        });

        let router = Router::new()
            .route(
                "/v18.0/{phone_number_id}/messages",
                post(handle_whatsapp_send),
            )
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> Vec<WhatsAppSentMessage> {
        self.state.captured.lock().unwrap().clone()
    }
}

async fn handle_whatsapp_send(
    State(state): State<Arc<WhatsAppState>>,
    Path(phone_number_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.captured.lock().unwrap().push(WhatsAppSentMessage {
        phone_number_id: phone_number_id.clone(),
        authorization,
        body,
    });
    Json(json!({
        "messaging_product": "whatsapp",
        "contacts": [{ "input": "stub", "wa_id": "stub" }],
        "messages": [{ "id": "wamid.STUB" }],
    }))
}
