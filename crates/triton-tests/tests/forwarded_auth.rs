//! Trust `X-Forwarded-Email` from a co-located oauth2-proxy sidecar
//! when explicitly opted in via `TRITON_TRUST_FORWARDED_AUTH=true`.
//!
//! Background — issue #67: the substrate deploys `dz-triton-api`
//! behind an `oauth2-proxy` sidecar (ADR-0011 / auth-portal-dz
//! idiom). The sidecar authenticates the operator against Vault's
//! `ops` realm, then forwards the request to Triton on the alloc's
//! loopback with `X-Forwarded-Email: <user>`. Without this fast-path
//! the SPA has to also send a `Bearer` token even though auth has
//! already happened at the sidecar — operators have to manually plant
//! a `dev-token` in localStorage to unstick the UI.
//!
//! Acceptance:
//! - With the flag ON and the header present, REST/MCP/A2A serve
//!   protected endpoints without an `Authorization` header.
//! - With the flag OFF, the header is ignored (default-deny posture
//!   for production builds that haven't opted in).
//! - With the flag ON but no header, the existing Bearer path is the
//!   only way in (so a forgotten sidecar config doesn't open a
//!   backdoor).
//!
//! No mocks: real spawned binary, real HTTP.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::StatusCode;
use triton_tests::TritonProcess;

const TRUST_ENV: &str = "TRITON_TRUST_FORWARDED_AUTH";
const FORWARDED_EMAIL: &str = "X-Forwarded-Email";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_email_admits_when_trust_enabled() {
    let env = HashMap::from([(TRUST_ENV.to_string(), "true".to_string())]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .header(FORWARDED_EMAIL, "ops-operator@example.com")
        .send()
        .await
        .expect("GET /v1/tools");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "trusted X-Forwarded-Email must admit the request when {TRUST_ENV}=true; \
         got {} (body: {:?})",
        resp.status(),
        resp.text().await.ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_email_rejected_when_trust_disabled() {
    // Default-deny: without the explicit opt-in env var, the header
    // must not be trusted, no matter who sets it.
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .header(FORWARDED_EMAIL, "attacker@example.com")
        .send()
        .await
        .expect("GET /v1/tools");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "X-Forwarded-Email without {TRUST_ENV} must NOT admit the request — \
         operator must explicitly opt in; got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_forwarded_email_still_requires_bearer() {
    // Trust mode on, but the request carries no forwarded header AND
    // no Authorization — must be rejected (no backdoor where simply
    // enabling trust mode without sending the header authenticates).
    let env = HashMap::from([(TRUST_ENV.to_string(), "true".to_string())]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .send()
        .await
        .expect("GET /v1/tools");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "no auth signal of any kind must still 401 even with {TRUST_ENV}=true; got {}",
        resp.status()
    );
}
