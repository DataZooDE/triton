//! Real OIDC issuer fixture for integration tests.
//!
//! CLAUDE.md §1 requires that identity tests run against a real
//! issuer rather than a Rust trait double. This module generates a
//! fresh Ed25519 keypair per test, spins up a tiny axum server that
//! serves the canonical OIDC discovery + JWKS endpoints, and exposes
//! a `sign_jwt` method so tests can mint tokens Triton actually
//! verifies. Restart-clean by construction (no on-disk state).

use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tokio::net::TcpListener;

pub struct TestIssuer {
    addr: SocketAddr,
    kid: String,
    encoding_key: EncodingKey,
}

impl TestIssuer {
    pub async fn start() -> Self {
        Self::start_inner(true).await
    }

    /// JWKS-only variant: serves `/jwks.json` but **no**
    /// `/.well-known/openid-configuration`. Models the mirror-image
    /// upstream-agent issuer of #100 — an agent that publishes its
    /// public keys on its internal FQDN without running a full OIDC
    /// discovery endpoint. A verifier configured with an explicit
    /// JWKS URL must work against this; one that insists on
    /// discovery must fail.
    pub async fn start_jwks_only() -> Self {
        Self::start_inner(false).await
    }

    async fn start_inner(serve_discovery: bool) -> Self {
        let signing_key = generate_ed25519_signing_key();
        let pem = signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("to_pkcs8_pem");
        let encoding_key = EncodingKey::from_ed_pem(pem.as_bytes()).expect("from_ed_pem");

        let verifying_key = signing_key.verifying_key();
        let public_b64 = URL_SAFE_NO_PAD.encode(verifying_key.as_bytes());

        let kid = format!("test-key-{}", random_hex_suffix());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().expect("local_addr");

        let issuer_url = format!("http://{addr}");
        let jwks_uri = format!("{issuer_url}/jwks.json");
        let discovery = json!({
            "issuer": issuer_url,
            "jwks_uri": jwks_uri,
            "id_token_signing_alg_values_supported": ["EdDSA"],
        });
        let jwks = json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": kid,
                "use": "sig",
                "alg": "EdDSA",
                "x": public_b64,
            }]
        });

        let mut router = Router::new().route(
            "/jwks.json",
            get(move || {
                let j = jwks.clone();
                async move { Json(j) }
            }),
        );
        if serve_discovery {
            router = router.route(
                "/.well-known/openid-configuration",
                get(move || {
                    let d = discovery.clone();
                    async move { Json(d) }
                }),
            );
        }

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            addr,
            kid,
            encoding_key,
        }
    }

    pub fn issuer_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Direct URL of the served JWKS document, for wiring
    /// `TRITON_OUTBOUND_JWKS_URL` without discovery (#100).
    pub fn jwks_url(&self) -> String {
        format!("http://{}/jwks.json", self.addr)
    }

    /// Sign a JWT with the issuer's private key. `claims` is whatever
    /// JSON object the test wants in the payload.
    pub fn sign_jwt(&self, claims: Value) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding_key).expect("sign jwt")
    }

    /// Produce an `alg=none` JWT for the negative test: header
    /// `{"alg":"none"}`, payload as given, empty signature segment.
    pub fn unsigned_jwt(&self, claims: Value) -> String {
        let header = json!({ "alg": "none", "typ": "JWT", "kid": self.kid });
        let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{h}.{p}.")
    }
}

fn generate_ed25519_signing_key() -> SigningKey {
    // Avoid the `rand` <-> `rand_core` version dance by getting
    // 32 bytes of entropy directly from the OS and seeding the key.
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("getrandom");
    SigningKey::from_bytes(&seed)
}

fn random_hex_suffix() -> String {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).expect("getrandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
