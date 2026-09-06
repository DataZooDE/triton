//! #286 — the upstream-agent auth contract, made executable.
//!
//! Triton authenticates callers and propagates identity; authorization
//! is the upstream agent's job (FR-U-6). A delegated model is only sound
//! if the contract is stated AND checkable, and until now it existed
//! only as a doc comment on `signer.rs` with nothing asserting it.
//!
//! This is a **reference upstream**: a real HTTP agent that verifies the
//! bearer the way FR-U-6 requires, and refuses what FR-U-6 says to
//! refuse. It is deliberately not a fixture that always says yes — the
//! point is to show the contract is implementable and that Triton mints
//! tokens which satisfy it.
//!
//! The clause under test is FR-U-6.2, audience pinning: an agent MUST
//! NOT accept a token minted for a different agent. That is what keeps
//! every hop a *named* audience rather than a replay, and it is the one
//! clause a careless upstream is most likely to skip, because ignoring
//! `aud` costs nothing and breaks nothing visibly.
//!
//! No mocks per CLAUDE.md §1: real binary, real RSA signing, real HTTP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use triton_tests::TritonProcess;

const KID: &str = "triton-test-signer";
const SIGNING_KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_key.pem"
));
const JWKS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_jwks.json"
));

/// What a conforming upstream decided about one request.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    Accepted {
        sub: String,
        tenant: String,
    },
    /// FR-U-6.2 — the token names a different agent.
    RefusedWrongAudience {
        presented: Vec<String>,
    },
    /// FR-U-6.4 — no tenant to authorize on.
    RefusedNoTenant,
}

/// A minimal upstream that actually implements FR-U-6.
///
/// It checks the claims the contract names. It does NOT verify the
/// signature — that is the JWKS plumbing, covered elsewhere, and
/// including it here would obscure the clause under test.
struct ReferenceUpstream {
    addr: std::net::SocketAddr,
    my_audience: String,
    verdicts: Arc<Mutex<Vec<Verdict>>>,
}

impl ReferenceUpstream {
    async fn start(my_audience: &str) -> Self {
        let verdicts = Arc::new(Mutex::new(Vec::new()));
        let seen = verdicts.clone();
        let aud = my_audience.to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                let aud = aud.clone();
                async move {
                    let bearer = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_string();
                    let claims = jwt_claims(&bearer);

                    // FR-U-6.2: pin `aud` to self.
                    let auds: Vec<String> = match &claims["aud"] {
                        Value::Array(a) => a
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect(),
                        Value::String(s) => vec![s.clone()],
                        _ => Vec::new(),
                    };
                    if !auds.iter().any(|a| a == &aud) {
                        seen.lock()
                            .unwrap()
                            .push(Verdict::RefusedWrongAudience { presented: auds });
                        return (axum::http::StatusCode::FORBIDDEN, "audience mismatch");
                    }
                    // FR-U-6.4: authorize on sub AND tenant.
                    let tenant = claims["tenant"].as_str().unwrap_or_default().to_string();
                    if tenant.is_empty() {
                        seen.lock().unwrap().push(Verdict::RefusedNoTenant);
                        return (axum::http::StatusCode::FORBIDDEN, "no tenant claim");
                    }
                    seen.lock().unwrap().push(Verdict::Accepted {
                        sub: claims["sub"].as_str().unwrap_or_default().to_string(),
                        tenant,
                    });
                    (axum::http::StatusCode::OK, "{}")
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self {
            addr,
            my_audience: my_audience.to_string(),
            verdicts,
        }
    }

    fn host_port(&self) -> String {
        self.addr.to_string()
    }

    fn verdicts(&self) -> Vec<Verdict> {
        self.verdicts.lock().unwrap().clone()
    }
}

fn jwt_claims(jwt: &str) -> Value {
    let Some(payload) = jwt.split('.').nth(1) else {
        return Value::Null;
    };
    URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn env_for(agent: &ReferenceUpstream, aud: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/manifest-whatsapp-test.yaml")
                .display()
                .to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("echo={}", agent.host_port()),
        ),
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), JWKS.to_string()),
        ("TRITON_JWT_KID".to_string(), KID.to_string()),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
        ("TRITON_STATIC_UPSTREAM_AUD".to_string(), aud.to_string()),
        // FR-U-6.4 requires a `tenant` claim. On this branch a token
        // carries one only when a deployment-static tenant is configured
        // — #283 makes the CALLER's tenant ship unconditionally, after
        // which this line stops being load-bearing for the per-caller
        // case. Set explicitly so the contract test exercises the
        // contract and not that separate defect.
        (
            "TRITON_STATIC_UPSTREAM_TENANT".to_string(),
            "acme".to_string(),
        ),
    ])
}

async fn dispatch(proc: &TritonProcess) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST echo")
        .status()
}

/// A token Triton mints for this agent satisfies the contract: the
/// agent's own audience is present, and `sub` and `tenant` are both
/// there to authorize on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conforming_upstream_accepts_a_token_minted_for_it() {
    let agent = ReferenceUpstream::start("agents-local").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&agent, "agents-local"))
            .await;

    assert!(dispatch(&proc).await.is_success());

    let verdicts = agent.verdicts();
    match verdicts.first() {
        Some(Verdict::Accepted { sub, tenant }) => {
            assert!(!sub.is_empty(), "FR-U-6.4: `sub` must be present");
            assert!(
                !tenant.is_empty(),
                "FR-U-6.4: `tenant` must be present — an agent that cannot see \
                 it has no tenant isolation and Triton cannot supply it on the \
                 agent's behalf (#283)"
            );
        }
        // A `RefusedNoTenant` here is not a test bug: it is #283. Until
        // the tenant claim ships unconditionally, a deployment that has
        // not configured a deployment-static tenant mints tokens with no
        // `tenant` at all, and FR-U-6.4 cannot be satisfied. The env
        // below sets one so this test exercises the contract rather than
        // that defect.
        other => panic!("expected an accepted verdict; got {other:?}"),
    }
}

/// FR-U-6.2, the clause a careless upstream skips: a token minted for a
/// DIFFERENT agent must be refused. Ignoring `aud` costs nothing and
/// breaks nothing visibly, which is exactly why the contract needs an
/// executable assertion rather than prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conforming_upstream_refuses_a_token_minted_for_another_agent() {
    // Triton mints for `agents-local`; this agent is somebody else.
    let agent = ReferenceUpstream::start("some-other-agent").await;
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&agent, "agents-local"))
            .await;

    let _ = dispatch(&proc).await;

    match agent.verdicts().first() {
        Some(Verdict::RefusedWrongAudience { presented }) => {
            assert!(
                presented.contains(&"agents-local".to_string()),
                "the refusal saw the audience Triton actually minted; got \
                 {presented:?}"
            );
            assert!(
                !presented.contains(&agent.my_audience),
                "and it was not this agent's own"
            );
        }
        other => panic!(
            "an upstream honouring FR-U-6.2 must refuse a token naming another \
             agent; got {other:?}"
        ),
    }
}
