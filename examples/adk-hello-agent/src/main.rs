//! A "hello world" upstream agent for Triton, built with adk-rust.
//!
//! Triton dispatches to this agent by POSTing the tool's args JSON to `/`
//! with `Authorization: Bearer <vault-minted-oidc>` (skill references/01).
//! The agent verifies the bearer, runs its adk-rust brain, and returns a
//! canonical A2UI `surface` (references/02). Triton wraps/builds the
//! response for whatever protocol the original caller used — REST, MCP,
//! A2A or a chat channel. This agent stays protocol-agnostic: it never
//! speaks adk-rust's own A2A server (that is the interface we replace).
//!
//! Run locally:  cargo run            (dev-token accepted, no issuer)
//!               ANTHROPIC_API_KEY=… cargo run   (live LLM brain)
//! Build to ship: cargo build --release --no-default-features

mod agent;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::agent::{Brain, LlmBrain, StaticBrain};

#[derive(Clone)]
struct AppState {
    brain: Arc<dyn Brain>,
    // When present, verify real OIDC tokens against this issuer
    // (references/04). When absent, the dev-token path applies.
    oidc_issuer: Option<String>,
    oidc_audience: Option<String>,
}

#[tokio::main]
async fn main() {
    // Pick the brain: a real adk-rust LlmAgent when an Anthropic key is
    // present, else a deterministic greeter so local/CI runs need no key.
    let brain: Arc<dyn Brain> = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => match LlmBrain::from_env() {
            Ok(b) => {
                eprintln!(
                    r#"{{"kind":"log","level":"info","msg":"using live adk-rust LLM brain"}}"#
                );
                Arc::new(b)
            }
            Err(e) => {
                eprintln!(
                    r#"{{"kind":"log","level":"error","msg":"LLM brain init failed, falling back to static","err":"{e}"}}"#
                );
                Arc::new(StaticBrain)
            }
        },
        _ => {
            eprintln!(
                r#"{{"kind":"log","level":"info","msg":"no ANTHROPIC_API_KEY — using deterministic static brain"}}"#
            );
            Arc::new(StaticBrain)
        }
    };

    let state = AppState {
        brain,
        oidc_issuer: std::env::var("AGENT_OIDC_ISSUER").ok(),
        oidc_audience: std::env::var("AGENT_OIDC_AUDIENCE").ok(),
    };

    let app = Router::new()
        // Triton always POSTs to `/` — it routed to us via Consul, so
        // there is no per-tool path on our side (references/01).
        .route("/", post(handle_tool_call))
        .route(
            "/healthz",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .with_state(state);

    let port: u16 = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!(r#"{{"kind":"log","level":"info","msg":"agent listening","addr":"{addr}"}}"#);
    axum::serve(listener, app).await.unwrap();
}

async fn handle_tool_call(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> impl IntoResponse {
    // 1. Verify Triton's bearer before doing any work (references/04).
    let principal = match verify_bearer(&state, &headers).await {
        Ok(sub) => sub,
        Err(reason) => {
            // Never log the token itself (references/09).
            eprintln!(
                r#"{{"kind":"log","level":"warn","msg":"auth rejected","reason":"{reason}"}}"#
            );
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "auth" }))).into_response();
        }
    };

    // 2. Run the tool. `args` is the object the dispatcher validated
    //    against the `hello` tool's schema: { "subject": "<string>" }.
    let subject = args
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("world");
    eprintln!(r#"{{"kind":"log","level":"info","msg":"hello invoked","who":"{principal}"}}"#);

    let greeting = match state.brain.greet(subject).await {
        Ok(g) => g,
        Err(e) => {
            eprintln!(r#"{{"kind":"log","level":"error","msg":"brain failed","err":"{e}"}}"#);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "brain" })),
            )
                .into_response();
        }
    };

    // 3. Return a canonical A2UI surface (references/02). Triton builds
    //    v0.8 / v0.9 / a chat PlatformMessage from this — we do NOT emit
    //    the versioned wire shape ourselves. The narration carries the
    //    greeting; the button lets any frontend re-invoke us.
    let surface = json!({
        "surface": {
            "components": [
                { "kind": "narration", "text": greeting },
                { "kind": "button",
                  "label": "Greet again",
                  "tool": "hello",
                  "args": { "subject": subject } }
            ]
        }
    });
    (StatusCode::OK, Json(surface)).into_response()
}

/// Returns the verified subject, or an error reason.
async fn verify_bearer(state: &AppState, headers: &HeaderMap) -> Result<String, String> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| "missing bearer".to_string())?;

    match &state.oidc_issuer {
        // Real OIDC path. This is a sketch — use the full verify recipe
        // (JWKS discovery + per-kid cache + rotation) from
        // substrate-platform/references/11-oidc-verification.md.
        Some(issuer) => verify_oidc(issuer, state.oidc_audience.as_deref(), bearer).await,

        // No issuer configured → dev / CI. Accept the literal dev-token,
        // but ONLY when this feature is compiled in. Release builds
        // (`--no-default-features`) reject everything here, so the
        // affordance cannot ship (ADR-10).
        None => {
            #[cfg(feature = "dev-token")]
            {
                if bearer == "dev-token" {
                    return Ok("dev-user".to_string());
                }
                Err("dev-token expected (no issuer configured)".to_string())
            }
            #[cfg(not(feature = "dev-token"))]
            {
                let _ = bearer;
                Err("no OIDC issuer configured and dev-token compiled out".to_string())
            }
        }
    }
}

/// Sketch of the real verification path. Replace with the cached recipe
/// from substrate-platform/references/11; this fetches JWKS on every
/// call, which is fine for a skeleton, not for production.
async fn verify_oidc(issuer: &str, audience: Option<&str>, token: &str) -> Result<String, String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};

    let discovery: Value = reqwest::get(format!("{issuer}/.well-known/openid-configuration"))
        .await
        .map_err(|e| format!("discovery: {e}"))?
        .json()
        .await
        .map_err(|e| format!("discovery json: {e}"))?;
    let jwks_uri = discovery["jwks_uri"].as_str().ok_or("no jwks_uri")?;
    let jwks: JwkSet = reqwest::get(jwks_uri)
        .await
        .map_err(|e| format!("jwks: {e}"))?
        .json()
        .await
        .map_err(|e| format!("jwks json: {e}"))?;

    let kid = decode_header(token)
        .map_err(|e| format!("header: {e}"))?
        .kid
        .ok_or("token missing kid")?;
    let key = jwks.find(&kid).ok_or("kid not in JWKS")?;
    let decoding = DecodingKey::from_jwk(key).map_err(|e| format!("decoding key: {e}"))?;

    // Triton's inbound allowlist (FR-I-3). EdDSA matches TestIssuer's
    // Ed25519 keys; widen to RS256/ES256 for the real substrate issuer.
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[issuer]);
    if let Some(aud) = audience {
        validation.set_audience(&[aud]);
    } else {
        validation.validate_aud = false;
    }

    #[derive(serde::Deserialize)]
    struct Claims {
        sub: String,
    }
    let data =
        decode::<Claims>(token, &decoding, &validation).map_err(|e| format!("verify: {e}"))?;
    Ok(data.claims.sub)
}
