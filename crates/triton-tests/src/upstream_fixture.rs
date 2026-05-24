//! Test fixtures for the upstream-router PR: tiny axum servers that
//! speak the actual Consul, Vault, and upstream-agent wire shapes.
//! No mocks per CLAUDE.md §1.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Fake Consul speaking `/v1/health/service/<service>?passing`.
///
/// Returns each registered service as a single healthy entry with
/// the Service.Address/Port the caller will dial.
pub struct FakeConsul {
    addr: SocketAddr,
}

impl FakeConsul {
    pub async fn start(services: &[(&str, String)]) -> Self {
        let table: Vec<(String, String)> = services
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let table = Arc::new(table);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();

        let router = Router::new().route(
            "/v1/health/service/{service}",
            get(move |Path(service): Path<String>| {
                let table = table.clone();
                async move {
                    let matches: Vec<Value> = table
                        .iter()
                        .filter(|(name, _)| name == &service)
                        .map(|(name, host_port)| {
                            let (host, port) = parse_host_port(host_port);
                            json!({
                                "Node": { "Address": host, "Node": "test" },
                                "Service": {
                                    "ID": format!("{name}-1"),
                                    "Service": name,
                                    "Address": host,
                                    "Port": port,
                                    "Tags": [format!("agent:{name}")],
                                },
                                "Checks": []
                            })
                        })
                        .collect();
                    Json(Value::Array(matches))
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn parse_host_port(host_port: &str) -> (String, u16) {
    let (h, p) = host_port.rsplit_once(':').expect("host:port");
    (h.to_string(), p.parse().expect("port"))
}

/// Fake Vault speaking `/v1/identity/oidc/token/<role>` for the
/// `agent-oidc-swap` role. Returns the configured opaque token
/// regardless of the inbound Vault token (we don't model auth in
/// the fake — just the wire shape).
pub struct FakeVault {
    addr: SocketAddr,
}

impl FakeVault {
    pub async fn start_minting(token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();

        let token = token.to_string();
        let router = Router::new().route(
            "/v1/identity/oidc/token/{role}",
            get(move |_: HeaderMap, Path(_role): Path<String>| {
                let token = token.clone();
                async move {
                    Json(json!({
                        "request_id": "00000000-0000-0000-0000-000000000000",
                        "lease_id": "",
                        "renewable": false,
                        "lease_duration": 0,
                        "data": {
                            "client_id": "agents",
                            "token": token,
                            "ttl": 300,
                        },
                        "wrap_info": null,
                        "warnings": null,
                        "auth": null,
                    }))
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Fake upstream agent. Several profiles:
/// * `echoing` — accepts anything, echoes the body back.
/// * `always_failing` — returns 500 on every request.
/// * `failing_then_recovering(n)` — fails the first `n` calls, then
///   recovers (used for circuit-breaker cooldown test).
pub struct FakeAgent {
    addr: SocketAddr,
    state: Arc<FakeAgentState>,
}

struct FakeAgentState {
    mode: Mutex<AgentMode>,
    bearers_seen: Mutex<Vec<String>>,
    hits: Mutex<u32>,
    failures_remaining: Mutex<u32>,
}

#[derive(Clone, Copy)]
enum AgentMode {
    Echo,
    AlwaysFail,
    FailingThenRecover,
}

impl FakeAgent {
    pub async fn start_echoing() -> Self {
        Self::start(AgentMode::Echo, 0).await
    }

    pub async fn start_always_failing() -> Self {
        Self::start(AgentMode::AlwaysFail, 0).await
    }

    pub async fn start_failing_then_recovering(fail_first: u32) -> Self {
        Self::start(AgentMode::FailingThenRecover, fail_first).await
    }

    async fn start(mode: AgentMode, fail_first: u32) -> Self {
        let state = Arc::new(FakeAgentState {
            mode: Mutex::new(mode),
            bearers_seen: Mutex::new(Vec::new()),
            hits: Mutex::new(0),
            failures_remaining: Mutex::new(fail_first),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();

        let router = Router::new()
            .route("/", any(handler))
            .route("/{*rest}", any(handler))
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn host_port(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    pub fn bearers_seen(&self) -> Vec<String> {
        self.state.bearers_seen.lock().unwrap().clone()
    }

    pub fn hits(&self) -> u32 {
        *self.state.hits.lock().unwrap()
    }

    pub fn reset_hits(&self) {
        *self.state.hits.lock().unwrap() = 0;
    }
}

async fn handler(
    State(state): State<Arc<FakeAgentState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    *state.hits.lock().unwrap() += 1;
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string())
        .unwrap_or_default();
    state.bearers_seen.lock().unwrap().push(bearer);

    let mode = *state.mode.lock().unwrap();
    let should_fail = match mode {
        AgentMode::Echo => false,
        AgentMode::AlwaysFail => true,
        AgentMode::FailingThenRecover => {
            let mut left = state.failures_remaining.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                true
            } else {
                false
            }
        }
    };
    if should_fail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "fake agent configured to fail" })),
        )
            .into_response();
    }

    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Json(json!({ "echoed": value })).into_response()
}
