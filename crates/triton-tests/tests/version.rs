//! FR-O-2 — `GET /version` returns the binary SHA + golden-image SHA
//! so operators can correlate a live alloc to a deployed image. The
//! substrate Packer step bakes `TRITON_IMAGE_SHA` into the Nomad job
//! env; `binary_sha` comes from `build.rs` at compile time.
//!
//! NFR-O-1 — `TRITON_*` env vars drive the settings struct. PR 3
//! adds `TRITON_ENV` and `TRITON_IMAGE_SHA` to the existing port set.
//!
//! No mocks: real binary, real env vars, real HTTP.

use std::collections::HashMap;
use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_endpoint_reports_build_and_runtime_metadata() {
    let extra_env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        (
            "TRITON_IMAGE_SHA".to_string(),
            "img-2026-05-24-test".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), extra_env).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/version"))
        .send()
        .await
        .expect("GET /version")
        .json()
        .await
        .expect("decode JSON");

    let binary_sha = body["binary_sha"].as_str().expect("binary_sha is a string");
    assert!(
        !binary_sha.is_empty(),
        "binary_sha must be stamped at build time, got empty string: {body}"
    );

    assert_eq!(
        body["image_sha"], "img-2026-05-24-test",
        "image_sha should reflect TRITON_IMAGE_SHA env var: {body}"
    );
    assert_eq!(
        body["env"], "nonprod",
        "env should reflect TRITON_ENV env var: {body}"
    );
    let pkg = body["package_version"]
        .as_str()
        .expect("package_version is a string");
    assert!(!pkg.is_empty(), "package_version present: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_flag_overrides_env_var() {
    // NFR-O-1 precedence: CLI flag beats env var.
    let env = HashMap::from([("TRITON_ENV".to_string(), "via-env".to_string())]);
    let args = vec!["--env".to_string(), "via-cli".to_string()];
    let proc = TritonProcess::spawn_with_args(Duration::from_secs(5), env, args).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/version"))
        .send()
        .await
        .expect("GET /version")
        .json()
        .await
        .expect("decode JSON");

    assert_eq!(
        body["env"], "via-cli",
        "--env flag must override TRITON_ENV: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_endpoint_defaults_when_image_sha_unset() {
    // With no TRITON_IMAGE_SHA set, image_sha is JSON null (the
    // alloc was not launched via the substrate Packer step — e.g.
    // local dev). env defaults to "local".
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), HashMap::new()).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/version"))
        .send()
        .await
        .expect("GET /version")
        .json()
        .await
        .expect("decode JSON");

    assert!(
        body["image_sha"].is_null(),
        "image_sha must be null when TRITON_IMAGE_SHA is unset: {body}"
    );
    assert_eq!(body["env"], "local", "env should default to local: {body}");
}
