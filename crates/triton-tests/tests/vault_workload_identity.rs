//! Vault auth via Nomad workload identity (no static token).
//!
//! Proves the substrate-preferred path: Triton reads a Nomad-issued
//! JWT from a file, POSTs it to `auth/<mount>/login`, receives a
//! Vault token, and uses *that* token for the per-call OIDC swap —
//! all with NO `TRITON_VAULT_TOKEN` set.
//!
//! No mocks: FakeVault serves the real login + OIDC wire shapes and
//! REQUIRES the login-issued token on the OIDC endpoint, so a green
//! dispatch is end-to-end proof the login token flowed through.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use triton_tests::TritonProcess;
use triton_tests::upstream_fixture::{FakeAgent, FakeConsul, FakeVault};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_authenticates_to_vault_via_workload_identity() {
    let agent = FakeAgent::start_echoing().await;
    let consul = FakeConsul::start(&[("stats", agent.host_port())]).await;
    // FakeVault issues `wi-issued-vault-token` at the login endpoint
    // and requires it on the OIDC swap, returning the agent token.
    let vault =
        FakeVault::start_workload_identity("wi-issued-vault-token", "vault-minted-agent-token")
            .await;

    // Nomad would write this JWT to $NOMAD_SECRETS_DIR/nomad_vault.jwt.
    let jwt_path = std::env::temp_dir().join(format!(
        "triton-wi-jwt-{}-{}.jwt",
        std::process::id(),
        agent.host_port().replace(':', "-")
    ));
    std::fs::write(&jwt_path, b"header.payload.sig").expect("write jwt");
    let jwt_path = jwt_path.to_str().unwrap().to_string();

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_CONSUL_URL".to_string(), consul.url()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        // No TRITON_VAULT_TOKEN — workload identity instead.
        ("TRITON_VAULT_JWT_PATH".to_string(), jwt_path),
        ("TRITON_VAULT_JWT_ROLE".to_string(), "triton".to_string()),
        (
            "TRITON_VAULT_AUTH_MOUNT".to_string(),
            "jwt-nomad".to_string(),
        ),
        (
            "TRITON_VAULT_OIDC_ROLE".to_string(),
            "agent-oidc-swap".to_string(),
        ),
        ("TRITON_UPSTREAM_TIMEOUT_MS".to_string(), "500".to_string()),
    ]);

    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/stats"))
        .bearer_auth("dev-token")
        .json(&json!({ "city": "Berlin" }))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "expected 2xx (login→mint→dispatch), got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["echoed"]["city"], "Berlin");

    // The agent saw the OIDC-minted token, which the OIDC endpoint
    // only returns when presented the login-issued Vault token —
    // so this also proves the workload-identity login succeeded.
    let seen = agent.bearers_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], "vault-minted-agent-token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_recovers_when_vault_token_is_revoked() {
    // Robustness review + codex follow-up: if the cached Vault token
    // is revoked server-side before its proactive refresh, the
    // consumer must invalidate it, re-login, and retry once. FakeVault
    // here 403s the FIRST OIDC swap (revocation) then accepts.
    let agent = FakeAgent::start_echoing().await;
    let consul = FakeConsul::start(&[("stats", agent.host_port())]).await;
    let vault = FakeVault::start_workload_identity_revoke_once(
        "wi-issued-vault-token",
        "vault-minted-agent-token",
    )
    .await;

    let jwt_path = std::env::temp_dir().join(format!(
        "triton-wi-revoke-jwt-{}-{}.jwt",
        std::process::id(),
        agent.host_port().replace(':', "-")
    ));
    std::fs::write(&jwt_path, b"header.payload.sig").expect("write jwt");
    let jwt_path = jwt_path.to_str().unwrap().to_string();

    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "nonprod".to_string()),
        ("TRITON_CONSUL_URL".to_string(), consul.url()),
        ("TRITON_VAULT_URL".to_string(), vault.url()),
        ("TRITON_VAULT_JWT_PATH".to_string(), jwt_path),
        ("TRITON_VAULT_JWT_ROLE".to_string(), "triton".to_string()),
        (
            "TRITON_VAULT_AUTH_MOUNT".to_string(),
            "jwt-nomad".to_string(),
        ),
        (
            "TRITON_VAULT_OIDC_ROLE".to_string(),
            "agent-oidc-swap".to_string(),
        ),
        ("TRITON_UPSTREAM_TIMEOUT_MS".to_string(), "500".to_string()),
    ]);

    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/stats"))
        .bearer_auth("dev-token")
        .json(&json!({ "city": "Berlin" }))
        .send()
        .await
        .expect("POST");
    assert!(
        resp.status().is_success(),
        "dispatch should recover after a revoked-token 403, got {}: {:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let body: serde_json::Value = resp.json().await.expect("decode");
    assert_eq!(body["result"]["echoed"]["city"], "Berlin");

    // Two OIDC swaps (the 403'd one + the retry) and two logins (the
    // initial + the forced re-login after invalidate) prove the
    // self-heal path actually ran.
    assert_eq!(vault.oidc_hits(), 2, "expected one failed swap + one retry");
    assert_eq!(
        vault.login_hits(),
        2,
        "expected initial login + a re-login after invalidate"
    );
    assert_eq!(agent.bearers_seen(), vec!["vault-minted-agent-token"]);
}
