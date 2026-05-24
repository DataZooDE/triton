//! `GET /v1/runtime` — anonymous discovery endpoint the Flutter
//! explorer SPA reads at boot to learn (a) which OIDC issuer to
//! redirect to for PKCE login and (b) which env/image it's looking
//! at. The endpoint is anonymous (no Bearer required) because the
//! SPA needs to hit it BEFORE the user has a token — that's the
//! whole point of discovery.
//!
//! Acceptance:
//!   * Reports `env`, `image_sha`, `package_version`, `binary_sha`
//!     so the explorer footer can pin a deploy.
//!   * Reports `oidc_issuer`, `oidc_audience`, `oidc_client_id`
//!     so PKCE has everything it needs at boot.
//!   * When the explorer isn't enabled (no
//!     `TRITON_EXPLORER_CLIENT_ID` set), `oidc_client_id` is null —
//!     the SPA can show a clear "operator hasn't registered me yet"
//!     message instead of failing the PKCE flow opaquely.
//!   * Anonymous: no `Authorization` header required.
//!
//! No mocks: real binary, real env, real HTTP.

use std::collections::HashMap;
use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_discovery_reports_oidc_and_image_metadata() {
    // OIDC verifier wants a working JWKS endpoint at boot when an
    // issuer is configured. Mount the real fixture and point at it.
    let issuer = triton_tests::TestIssuer::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        (
            "TRITON_IMAGE_SHA".to_string(),
            "img-2026-05-24-test".to_string(),
        ),
        ("TRITON_OIDC_ISSUER".to_string(), issuer.issuer_url()),
        (
            "TRITON_OIDC_AUDIENCE".to_string(),
            "agents-nonprod".to_string(),
        ),
        (
            "TRITON_EXPLORER_CLIENT_ID".to_string(),
            "triton-explorer".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/runtime"))
        .send()
        .await
        .expect("GET /v1/runtime")
        .json()
        .await
        .expect("decode JSON");

    assert_eq!(body["env"], "nonprod", "env field: {body}");
    assert_eq!(
        body["image_sha"], "img-2026-05-24-test",
        "image_sha field: {body}"
    );
    assert_eq!(
        body["oidc_issuer"],
        serde_json::Value::String(issuer.issuer_url()),
        "oidc_issuer field: {body}"
    );
    assert_eq!(
        body["oidc_audience"], "agents-nonprod",
        "oidc_audience field: {body}"
    );
    assert_eq!(
        body["oidc_client_id"], "triton-explorer",
        "oidc_client_id field: {body}"
    );
    let pkg = body["package_version"]
        .as_str()
        .expect("package_version is a string");
    assert!(!pkg.is_empty(), "package_version present: {body}");
    let bsha = body["binary_sha"].as_str().expect("binary_sha is a string");
    assert!(!bsha.is_empty(), "binary_sha present: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_discovery_is_anonymous() {
    // No Bearer header sent. /v1/runtime must respond before the SPA
    // has a token — that's the discovery handshake.
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/v1/runtime"))
        .send()
        .await
        .expect("GET /v1/runtime");
    assert!(
        resp.status().is_success(),
        "anonymous discovery must succeed, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_discovery_omits_client_id_when_unset() {
    // When TRITON_EXPLORER_CLIENT_ID isn't set, the SPA hasn't been
    // operator-enabled in this env — the endpoint should signal that
    // by returning null for client_id (not a string, not an empty
    // string — null is the explicit "not configured" marker).
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/runtime"))
        .send()
        .await
        .expect("GET /v1/runtime")
        .json()
        .await
        .expect("decode JSON");

    assert!(
        body["oidc_client_id"].is_null(),
        "oidc_client_id should be null when env unset: {body}"
    );
    assert!(
        body["oidc_issuer"].is_null(),
        "oidc_issuer should be null when env unset: {body}"
    );
    assert!(
        body["oidc_audience"].is_null(),
        "oidc_audience should be null when env unset: {body}"
    );
}
