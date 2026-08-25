//! Several OIDC issuer/audience pairs accepted at one boundary (#213).
//!
//! Why this exists: one agent can be reached by callers from different
//! identity providers at once — Google for humans, Entra for a Microsoft
//! agent platform — and a single-pair boundary forces a choice between
//! them. These pairs are ALTERNATIVES, not layers, so the property that
//! matters is that adding pair 2 does not weaken pair 1.
//!
//! No mocks: two real `TestIssuer` fixtures, each with its own keypair,
//! discovery document and JWKS, served over real sockets. Tokens are
//! really signed and really verified.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};
use triton_tests::TestIssuer;

const AUD_A: &str = "audience-google-side";
const AUD_B: &str = "audience-entra-side";

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

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn claims(iss: &str, aud: &str, sub: &str) -> Value {
    json!({
        "iss": iss,
        "aud": aud,
        "sub": sub,
        "exp": now() + 600,
        "iat": now() - 5,
    })
}

/// Serve the trio on a real socket with the given opts; returns the base URL.
async fn serve(opts: EmbedOpts) -> String {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(reg), "test".to_string()));
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn call(base: &str, token: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/v1/tools/echo"))
        .bearer_auth(token)
        .json(&json!({ "hello": "world" }))
        .send()
        .await
        .expect("POST")
        .status()
}

/// Both configured pairs are accepted, independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn either_configured_issuer_is_accepted() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    let ta = a.sign_jwt(claims(&a.issuer_url(), AUD_A, "alice"));
    let tb = b.sign_jwt(claims(&b.issuer_url(), AUD_B, "bob"));

    assert_eq!(call(&base, &ta).await, 200, "pair 1 must be accepted");
    assert_eq!(call(&base, &tb).await, 200, "pair 2 must be accepted");
}

/// THE security property. Selecting a verifier by the token's own
/// unverified `iss` must not let a token launder itself: a token really
/// signed by A, but claiming to come from B, is offered to B's verifier
/// and dies there on the signature. Selection is routing, not trust.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_cannot_launder_itself_by_claiming_another_issuer() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    // Signed by A's key, but every claim says it is B's.
    let forged = a.sign_jwt(claims(&b.issuer_url(), AUD_B, "mallory"));
    assert_eq!(
        call(&base, &forged).await,
        401,
        "a token signed by the wrong issuer's key must be refused"
    );
}

/// Cross-pair audience confusion: a token genuinely from A, with A's
/// issuer, but carrying pair 2's audience. A's verifier requires A's
/// audience, so it is refused — adding a pair must not create a
/// wildcard audience.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_other_pairs_audience_is_not_accepted() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    let wrong_aud = a.sign_jwt(claims(&a.issuer_url(), AUD_B, "alice"));
    assert_eq!(
        call(&base, &wrong_aud).await,
        401,
        "pair 1's issuer with pair 2's audience must be refused"
    );
}

/// An issuer nobody configured is refused without a network call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfigured_issuer_is_refused() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    let stranger = a.sign_jwt(claims("https://not-configured.test", AUD_A, "eve"));
    assert_eq!(call(&base, &stranger).await, 401);
}

/// Configuring ANY pair closes the dev-token path, exactly as a single
/// pair does (ADR-10: OIDC always wins). Multi-issuer must not reopen
/// the backdoor that `oidc.is_some()` used to close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_issuer_still_closes_the_dev_token_path() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    assert_eq!(call(&base, "dev-token").await, 401);
}

/// `/v1/runtime` keeps its published shape — `oidc_issuer` still names
/// pair 1, which is what ADR-0017's verification reads — and gains the
/// full list so multi-issuer is observable rather than inferred.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_reports_pair_one_as_before_plus_the_full_list() {
    let a = TestIssuer::start().await;
    let b = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None).oidc(
        b.issuer_url(),
        AUD_B,
        None,
    ))
    .await;

    let body: Value = reqwest::get(format!("{base}/v1/runtime"))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");

    assert_eq!(body["oidc_issuer"], a.issuer_url(), "pair 1 unchanged");
    assert_eq!(body["oidc_audience"], AUD_A);
    let providers = body["oidc_providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[1]["issuer"], b.issuer_url());
    assert_eq!(providers[1]["audience"], AUD_B);
}

/// A single-pair deployment keeps every pre-existing field with its
/// pre-existing meaning, and also lists its one pair — so a client can
/// always read `oidc_providers` without special-casing the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_pair_still_reports_the_legacy_scalars() {
    let a = TestIssuer::start().await;
    let base = serve(EmbedOpts::dev().oidc(a.issuer_url(), AUD_A, None)).await;

    let body: Value = reqwest::get(format!("{base}/v1/runtime"))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");

    // The contract ADR-0017 verification reads, unchanged.
    assert_eq!(body["oidc_issuer"], a.issuer_url());
    assert_eq!(body["oidc_audience"], AUD_A);
    assert_eq!(
        body["oidc_client_id"], AUD_A,
        "client id defaults to the audience"
    );

    let providers = body["oidc_providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 1, "the one pair is listed too: {body}");
    assert_eq!(providers[0]["issuer"], a.issuer_url());
}
