//! `triton-demo-agent` — a tiny upstream agent for the Triton
//! substrate demo. Triton discovers it in Consul as
//! `tag:agent:demo-stats`, mints a per-call OIDC token via Vault
//! (`agent-oidc-swap`), and POSTs the tool args to `/`. We return a
//! canonical A2UI `surface` (text + narration + dashboard); Triton's
//! builders project it to v0.8 / v0.9 / a chat PlatformMessage.
//!
//! Auth (demo posture): we require a Bearer to be present and log
//! that it arrived, but do NOT cryptographically verify it yet.
//! Hardening — full JWKS verification against the substrate issuer
//! (substrate-platform/references/11) — is a follow-up before any
//! non-demo use. The token never appears in logs.
//!
//! Endpoints: `POST /` (the tool), `GET /healthz`.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    // The substrate OIDC issuer (Consul KV `substrate/oidc/issuer_url`,
    // templated into the env by Nomad) is read for the startup log;
    // strict JWKS verification against it is the documented follow-up.
    let issuer_configured = std::env::var("AGENT_OIDC_ISSUER")
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let app = Router::new().route("/", post(handle_tool_call)).route(
        "/healthz",
        get(|| async { Json(json!({ "status": "ok" })) }),
    );

    let port: u16 = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!(
        r#"{{"kind":"log","level":"info","msg":"demo-agent listening","addr":"{addr}","oidc_issuer_configured":{issuer_configured}}}"#
    );
    // Graceful shutdown on SIGTERM (Nomad alloc stop).
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .unwrap();
}

async fn handle_tool_call(headers: HeaderMap, Json(args): Json<Value>) -> impl IntoResponse {
    // Require a Bearer (Triton always mints + sends one). We do NOT
    // log the token (FR-AU-3). Strict JWKS verification is a follow-up.
    let has_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    if !has_bearer {
        eprintln!(r#"{{"kind":"log","level":"warn","msg":"demo-agent: missing bearer"}}"#);
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "auth" }))).into_response();
    }
    let window = args
        .get("window")
        .and_then(Value::as_str)
        .unwrap_or("last 24h");
    eprintln!(
        r#"{{"kind":"log","level":"info","msg":"demo-agent: dispatch","window":"{window}"}}"#
    );

    // Canonical A2UI surface (references/02): text + narration +
    // dashboard. Triton wraps/builds this for the inbound caller.
    let surface = json!({
        "surface": {
            "components": [
                { "kind": "text", "value": format!("Demo stats — {window}") },
                { "kind": "narration",
                  "text": "Synthetic numbers from the Triton demo upstream agent (tag:agent:demo-stats)." },
                { "kind": "dashboard",
                  "title": "Gateway demo",
                  "tiles": [
                      { "label": "requests", "value": "1,284", "trend": "+12% vs prior" },
                      { "label": "p95 latency", "value": "84 ms" },
                      { "label": "errors", "value": "3", "trend": "-2 vs prior" }
                  ] }
            ]
        }
    });
    (StatusCode::OK, Json(surface)).into_response()
}
