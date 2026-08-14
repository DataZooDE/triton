//! The embedded host's inbound identity boundary.
//!
//! `EmbedOpts::oidc` is what makes triton-embed usable outside `cargo run`:
//! before it existed `router()` hardcoded `IdentityProvider::new(None)`, so a
//! release build (`--no-default-features`, dev-token compiled out) accepted
//! nothing at all while `/healthz` kept answering 200. These tests pin the two
//! properties that failure mode needs:
//!
//!   1. configuring OIDC **closes** the dev-token path, even in this build
//!      where the `dev-token` feature IS on (ADR-10: OIDC always wins, so an
//!      accidentally-present feature flag can never reopen a backdoor); and
//!   2. `/v1/runtime` reports the issuer, so "is identity wired?" is an
//!      observable fact rather than something inferred from a 401.
//!
//! No mocks and no network: a real `axum::serve` over a real socket. The
//! issuer here is never contacted because every token is rejected before
//! JWKS lookup (bad header / wrong shape), which is exactly the boundary
//! under test.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};

const ISSUER: &str = "https://accounts.google.com";
const AUDIENCE: &str = "test-client-id.apps.googleusercontent.com";

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        Ok(json!({ "echoed": args }))
    }
}

async fn boot(opts: EmbedOpts) -> String {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), "test"));
    let app = router(dispatcher, &opts);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// With an OIDC issuer configured, the dev token stops working — on every
/// surface of the trio, not just REST. This is the regression that matters:
/// a host meant to be Google-authenticated must not also accept a literal
/// string that ships in the default feature set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_configured_closes_the_dev_token_path() {
    let base = boot(EmbedOpts::dev().oidc(ISSUER, AUDIENCE, None)).await;
    let http = reqwest::Client::new();

    let rest = http
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": "should-not-pass" }))
        .send()
        .await
        .expect("REST request");
    assert_eq!(
        rest.status(),
        401,
        "dev-token must be refused once OIDC is configured (REST)"
    );

    let a2a = http
        .post(format!("{base}/a2a/message:send"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": { "role": "user", "parts": [] } }))
        .send()
        .await
        .expect("A2A request");
    assert_eq!(
        a2a.status(),
        401,
        "dev-token must be refused once OIDC is configured (A2A)"
    );
}

/// A garbage bearer is rejected too — and, importantly, rejected rather than
/// hanging: the verifier only reaches JWKS discovery for a well-formed header
/// with an unknown `kid`, so an unreachable issuer cannot stall the boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_bearer_is_rejected() {
    let base = boot(EmbedOpts::dev().oidc(ISSUER, AUDIENCE, None)).await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth("not.a.jwt")
        .json(&json!({}))
        .send()
        .await
        .expect("REST request");
    assert_eq!(res.status(), 401, "malformed JWT must be refused");
}

/// `/v1/runtime` advertises the issuer/audience/client_id so a caller can
/// discover how to authenticate — and so an operator can tell a configured
/// host from an unconfigured one without guessing from a 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_advertises_the_issuer() {
    let base = boot(EmbedOpts::dev().oidc(ISSUER, AUDIENCE, None)).await;
    let http = reqwest::Client::new();

    let runtime: Value = http
        .get(format!("{base}/v1/runtime"))
        .send()
        .await
        .expect("runtime")
        .json()
        .await
        .expect("json");

    assert_eq!(runtime["oidc_issuer"], ISSUER, "issuer at /v1/runtime");
    assert_eq!(runtime["oidc_audience"], AUDIENCE, "audience at /v1/runtime");
    // Defaults to the audience: for Google the OAuth client ID *is* the aud.
    assert_eq!(
        runtime["oidc_client_id"], AUDIENCE,
        "client_id defaults to the audience"
    );
}

/// Without OIDC the host still reports `null`, and the dev-token path stays
/// open in this (feature-enabled) build. Pins the default so the change above
/// is additive: `cargo run` keeps working exactly as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_oidc_the_issuer_is_null_and_dev_token_works() {
    let base = boot(EmbedOpts::dev()).await;
    let http = reqwest::Client::new();

    let runtime: Value = http
        .get(format!("{base}/v1/runtime"))
        .send()
        .await
        .expect("runtime")
        .json()
        .await
        .expect("json");
    assert!(
        runtime["oidc_issuer"].is_null(),
        "unconfigured host reports a null issuer, got {}",
        runtime["oidc_issuer"]
    );

    let rest = http
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth("dev-token")
        .json(&json!({ "marker": "ok" }))
        .send()
        .await
        .expect("REST request");
    assert!(
        rest.status().is_success(),
        "dev-token still works when no OIDC is configured, got {}",
        rest.status()
    );
}
