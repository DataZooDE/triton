//! OIDC identity boundary — FR-I-1..3. The test fixture
//! [`triton_tests::TestIssuer`] spins up a real local HTTP server
//! that serves `/.well-known/openid-configuration` + `/jwks.json`
//! and signs JWTs with a fresh Ed25519 keypair. The binary verifies
//! against the real wire shape — no mocks, no trait doubles.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use triton_tests::{TestIssuer, TritonProcess};

const AUDIENCE: &str = "agents-nonprod";

fn env_for(issuer: &TestIssuer) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        ("TRITON_OIDC_AUDIENCE".to_string(), AUDIENCE.to_string()),
    ])
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_oidc_token_authorises_dispatch() {
    let issuer = TestIssuer::start().await;
    let token = issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "user-42",
        "aud": AUDIENCE,
        "exp": now() + 60,
        "iat": now(),
        "tenant": "acme",
        "scope": "tools:invoke"
    }));
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth(&token)
        .json(&json!({ "message": "hi from oidc" }))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["echo"], "hi from oidc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_token_rejected() {
    let issuer = TestIssuer::start().await;
    // Push exp well past jsonwebtoken's default 60s leeway so the
    // assertion exercises real expiration logic and not boundary
    // jitter.
    let token = issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "user-42",
        "aud": AUDIENCE,
        "exp": now() - 3600,
        "iat": now() - 7200,
    }));
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth(&token)
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_audience_rejected() {
    let issuer = TestIssuer::start().await;
    let token = issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "user-42",
        "aud": "agents-prod",
        "exp": now() + 60,
        "iat": now(),
    }));
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth(&token)
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alg_none_rejected() {
    // FR-I-3: alg=none MUST be rejected.
    let issuer = TestIssuer::start().await;
    let unsigned = issuer.unsigned_jwt(json!({
        "iss": issuer.issuer_url(),
        "sub": "evil",
        "aud": AUDIENCE,
        "exp": now() + 60,
    }));
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth(&unsigned)
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_token_rejected_when_oidc_configured() {
    // Even with dev-token feature compiled in, when an OIDC issuer
    // is configured the dev-token MUST NOT bypass real verification.
    let issuer = TestIssuer::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_for(&issuer)).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_token_still_works_without_oidc() {
    // PR 1-7 behaviour: no OIDC issuer configured → dev-token path
    // is the only verifier; existing tests rely on this.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());
}
