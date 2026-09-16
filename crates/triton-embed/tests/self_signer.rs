//! The embedded host as an OIDC issuer for its own static upstreams (#284).
//!
//! `triton-bin` has always been able to sign per-call RS256 JWTs to a static
//! upstream and serve the matching discovery + JWKS, but the EMBEDDED host
//! could not: `router()` hard-coded `RestState.oidc_signer = None`, so an
//! embedding host that reaches a static upstream (the DataZoo agent →
//! anofox-evolve) had no way to be an issuer the upstream could verify against.
//!
//! `EmbedOpts::oidc_signer` closes that: when set, the REST adapter serves
//! `/.well-known/openid-configuration` + `/.well-known/jwks.json` from it; when
//! unset, those routes stay 404 exactly as before.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::{Dispatcher, ToolRegistry};
use triton_embed::{EmbedOpts, router};
use triton_identity::JwtSigner;

const ISSUER: &str = "https://agent-lab.data-zoo.de";

/// A throwaway signer + its JWKS, mirroring triton-identity's own signer tests
/// (a 2048-bit key; this module never generates keys itself in production).
fn test_signer() -> Arc<JwtSigner> {
    let mut rng = rand::thread_rng();
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
    let pem = private.to_pkcs8_pem(LineEnding::LF).expect("pkcs8 pem");
    let public = RsaPublicKey::from(&private);
    let b64 = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
    let kid = "embed-self-signer-1";
    let jwks = json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": kid,
            "n": b64(&public.n().to_bytes_be()),
            "e": b64(&public.e().to_bytes_be()),
        }]
    });
    Arc::new(JwtSigner::from_rsa_pem(pem.as_bytes(), kid, ISSUER, jwks).expect("build signer"))
}

async fn boot(opts: EmbedOpts) -> String {
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(ToolRegistry::new()),
        "test",
        DispatchControls::unenforced(),
    ));
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// With a self-signer set, the embedded host serves OIDC discovery + JWKS so an
/// upstream can discover it as an issuer and verify the RS256 JWTs it mints.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_signer_serves_discovery_and_jwks() {
    let base = boot(EmbedOpts::dev().oidc_signer(test_signer())).await;
    let http = reqwest::Client::new();

    let disco: Value = http
        .get(format!("{base}/.well-known/openid-configuration"))
        .send()
        .await
        .expect("discovery request")
        .error_for_status()
        .expect("discovery is 200")
        .json()
        .await
        .expect("discovery json");
    assert_eq!(disco["issuer"], ISSUER);
    assert_eq!(
        disco["jwks_uri"],
        format!("{ISSUER}/.well-known/jwks.json"),
        "an upstream follows jwks_uri from discovery"
    );
    assert_eq!(
        disco["id_token_signing_alg_values_supported"][0], "RS256",
        "the agent verifier pins RS256"
    );

    // The jwks_uri the discovery doc advertises must itself serve the key.
    let jwks: Value = http
        .get(format!("{base}/.well-known/jwks.json"))
        .send()
        .await
        .expect("jwks request")
        .error_for_status()
        .expect("jwks is 200")
        .json()
        .await
        .expect("jwks json");
    assert_eq!(jwks["keys"][0]["kid"], "embed-self-signer-1");
    assert_eq!(jwks["keys"][0]["alg"], "RS256");
}

/// Without a self-signer (the default), those routes stay 404 — the embedded
/// host is not an issuer, exactly as before this wiring existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_signer_leaves_the_well_known_routes_404() {
    let base = boot(EmbedOpts::dev()).await;
    let http = reqwest::Client::new();

    for path in [
        "/.well-known/openid-configuration",
        "/.well-known/jwks.json",
    ] {
        let status = http
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(status, 404, "{path} must be 404 without a self-signer");
    }
}
