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

/// Fake Vault speaking either the OIDC swap endpoint
/// (`/v1/identity/oidc/token/<role>`, used by the PR 9 upstream
/// router) or the KV v2 read endpoint (`GET /v1/<path>`, used by
/// the PR 16 secret resolver). CLAUDE.md §1 admits "tiny in-repo
/// HTTP fakes that speak the actual wire protocol" as not-mocks.
pub struct FakeVault {
    addr: SocketAddr,
}

impl FakeVault {
    /// OIDC-swap-only fake. Returns `token` on every
    /// `/v1/identity/oidc/token/{role}` request (auth not modelled).
    pub async fn start_minting(token: &'static str) -> Self {
        Self::start(VaultConfig {
            oidc_token: Some(token.to_string()),
            kv_v2: Vec::new(),
            expected_token: None,
            login_token: None,
        })
        .await
    }

    /// Workload-identity fake: serves `POST /v1/auth/<mount>/login`
    /// issuing `issued_token`, and the OIDC swap endpoint which both
    /// REQUIRES that token (proving Triton logged in and presented
    /// the workload-identity token) and returns `agent_oidc_token`.
    /// No KV route (keeps the router free of a wildcard that would
    /// clash with the static login/oidc routes).
    pub async fn start_workload_identity(issued_token: &str, agent_oidc_token: &str) -> Self {
        Self::start(VaultConfig {
            oidc_token: Some(agent_oidc_token.to_string()),
            kv_v2: Vec::new(),
            expected_token: Some(issued_token.to_string()),
            login_token: Some(issued_token.to_string()),
        })
        .await
    }

    /// KV-v2-only fake. Serves the listed `(path, fields)` pairs at
    /// `GET /v1/<path>` and requires the right `X-Vault-Token`
    /// header on every request so the resolver's auth wiring is
    /// exercised. `path` must include the KV v2 `data/` segment
    /// (e.g. `kv/data/apps/dz/triton/test/telegram`) because the
    /// manifest's `vault://<path>#<field>` refs include it verbatim.
    pub async fn start_kv_v2(expected_token: &str, entries: &[(&str, &[(&str, &str)])]) -> Self {
        let kv_v2 = entries
            .iter()
            .map(|(path, fields)| {
                let map = fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<Vec<_>>();
                (path.to_string(), map)
            })
            .collect();
        Self::start(VaultConfig {
            oidc_token: None,
            kv_v2,
            expected_token: Some(expected_token.to_string()),
            login_token: None,
        })
        .await
    }

    async fn start(cfg: VaultConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(cfg);

        let mut router: Router = Router::new();
        // Workload-identity login: issue `login_token` to anyone who
        // POSTs a (non-empty) jwt. We don't verify the JWT — the test
        // only needs to prove Triton read the file, logged in, and
        // then USED the issued token downstream.
        if state.login_token.is_some() {
            let login_state = state.clone();
            router = router.route(
                "/v1/auth/{mount}/login",
                axum::routing::post(move |Path(_mount): Path<String>, body: String| {
                    let token = login_state.login_token.clone().unwrap();
                    async move {
                        let _ = &body;
                        Json(json!({
                            "auth": { "client_token": token, "lease_duration": 3600 }
                        }))
                    }
                }),
            );
        }
        if state.oidc_token.is_some() {
            let oidc_state = state.clone();
            router = router.route(
                "/v1/identity/oidc/token/{role}",
                get(move |headers: HeaderMap, Path(_role): Path<String>| {
                    let oidc_state = oidc_state.clone();
                    async move {
                        // When configured, require the (login-issued)
                        // token — proves the mint uses the WI token.
                        if let Some(expected) = &oidc_state.expected_token {
                            let presented = headers
                                .get("x-vault-token")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            if presented != expected {
                                return (StatusCode::FORBIDDEN, "wrong vault token")
                                    .into_response();
                            }
                        }
                        let token = oidc_state.oidc_token.clone().unwrap();
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
                        .into_response()
                    }
                }),
            );
        }
        if !state.kv_v2.is_empty() {
            let kv_state = state.clone();
            router = router.route(
                "/v1/{*path}",
                get(move |headers: HeaderMap, Path(path): Path<String>| {
                    let kv_state = kv_state.clone();
                    async move { kv_v2_handler(kv_state, headers, path).await }
                }),
            );
        }

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

struct VaultConfig {
    oidc_token: Option<String>,
    kv_v2: Vec<(String, Vec<(String, String)>)>,
    expected_token: Option<String>,
    /// When set, serve `POST /v1/auth/<mount>/login` issuing this
    /// token (workload-identity flow).
    login_token: Option<String>,
}

async fn kv_v2_handler(
    state: Arc<VaultConfig>,
    headers: HeaderMap,
    requested_path: String,
) -> axum::response::Response {
    if let Some(expected) = &state.expected_token {
        let presented = headers
            .get("x-vault-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if presented != expected {
            return (StatusCode::FORBIDDEN, "wrong vault token").into_response();
        }
    }
    let Some((_, fields)) = state.kv_v2.iter().find(|(p, _)| p == &requested_path) else {
        return (StatusCode::NOT_FOUND, "no such secret").into_response();
    };
    let mut data = serde_json::Map::new();
    for (k, v) in fields {
        data.insert(k.clone(), Value::String(v.clone()));
    }
    Json(json!({
        "request_id": "00000000-0000-0000-0000-000000000000",
        "lease_id": "",
        "renewable": false,
        "lease_duration": 0,
        "data": {
            "data": Value::Object(data),
            "metadata": {
                "created_time": "2026-05-24T00:00:00Z",
                "destroyed": false,
                "version": 1,
            }
        },
        "wrap_info": null,
        "warnings": null,
        "auth": null,
        "mount_type": "kv",
    }))
    .into_response()
}

/// Fake upstream agent. Several profiles:
/// * `echoing` — accepts anything, echoes the body back.
/// * `always_failing` — returns 500 on every request.
/// * `failing_then_recovering(n)` — fails the first `n` calls, then
///   recovers (used for circuit-breaker cooldown test).
/// * `returning(json)` — responds with a fixed JSON body on every
///   request (used by the `upstream` identity-resolver test, where the
///   resolver agent returns a `{sub, scopes, tenant}` principal).
pub struct FakeAgent {
    addr: SocketAddr,
    state: Arc<FakeAgentState>,
}

struct FakeAgentState {
    mode: Mutex<AgentMode>,
    bearers_seen: Mutex<Vec<String>>,
    hits: Mutex<u32>,
    failures_remaining: Mutex<u32>,
    /// Fixed response body for `AgentMode::Returning`.
    fixed_response: Option<Value>,
}

#[derive(Clone, Copy)]
enum AgentMode {
    Echo,
    AlwaysFail,
    FailingThenRecover,
    Returning,
}

impl FakeAgent {
    pub async fn start_echoing() -> Self {
        Self::start(AgentMode::Echo, 0, None).await
    }

    pub async fn start_always_failing() -> Self {
        Self::start(AgentMode::AlwaysFail, 0, None).await
    }

    pub async fn start_failing_then_recovering(fail_first: u32) -> Self {
        Self::start(AgentMode::FailingThenRecover, fail_first, None).await
    }

    /// Respond with `body` (status 200) on every request, ignoring the
    /// request body. The upstream router returns this verbatim as the
    /// tool result.
    pub async fn start_returning(body: Value) -> Self {
        Self::start(AgentMode::Returning, 0, Some(body)).await
    }

    async fn start(mode: AgentMode, fail_first: u32, fixed_response: Option<Value>) -> Self {
        let state = Arc::new(FakeAgentState {
            mode: Mutex::new(mode),
            bearers_seen: Mutex::new(Vec::new()),
            hits: Mutex::new(0),
            failures_remaining: Mutex::new(fail_first),
            fixed_response,
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
        AgentMode::Echo | AgentMode::Returning => false,
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

    if let AgentMode::Returning = mode {
        let body = state.fixed_response.clone().unwrap_or(Value::Null);
        return Json(body).into_response();
    }

    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Json(json!({ "echoed": value })).into_response()
}
