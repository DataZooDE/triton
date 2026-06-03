//! RS256 JWT signer for Consul-less (`StaticUpstream`) dispatch.
//!
//! Workload→workload auth without Vault. In static-upstream mode Triton has no
//! Vault to mint the per-call agent OIDC token (FR-U-2), so it would fall back
//! to a literal `dev-token` bearer — which production agents (built
//! `--no-default-features`, dev-token compiled out, ADR-10) reject. This signer
//! lets Triton mint a short-lived **RS256** JWT per upstream call and serve the
//! matching JWKS + OIDC discovery, so an agent verifies it through its normal
//! `AGENT_OIDC_ISSUER` path exactly as it would a Vault-minted token. RS256 is
//! deliberate: the reference agent verifier pins `Algorithm::RS256`.
//!
//! The signing key + public JWKS are supplied by config (the operator generates
//! both from one RSA keypair, same `kid`). A *shared* key across Triton
//! instances keeps the served JWKS consistent behind a load balancer, so an
//! agent verifies regardless of which instance answered its JWKS fetch. This
//! module only signs and serves; it never generates or persists keys.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};

/// NFR-S-3: minted upstream tokens are short-lived (≤ 5 min), matching the
/// Vault per-call swap's TTL cap.
const MAX_TTL: Duration = Duration::from_secs(300);

/// Signs short-lived RS256 JWTs for upstream dispatch and exposes the matching
/// JWKS + OIDC discovery document.
pub struct JwtSigner {
    encoding_key: EncodingKey,
    kid: String,
    issuer: String,
    jwks: Value,
}

impl JwtSigner {
    /// Build from an RSA private key PEM (PKCS#8 or PKCS#1) and the matching
    /// public JWKS. `issuer` is the URL agents reach for discovery/JWKS and is
    /// stamped as the token `iss`. `kid` must match the `kid` in `jwks` and is
    /// set in every signed token's header so the verifier selects the right key.
    pub fn from_rsa_pem(
        pem: &[u8],
        kid: impl Into<String>,
        issuer: impl Into<String>,
        jwks: Value,
    ) -> Result<Self, String> {
        let encoding_key =
            EncodingKey::from_rsa_pem(pem).map_err(|e| format!("invalid RSA signing key: {e}"))?;
        Ok(Self {
            encoding_key,
            kid: kid.into(),
            issuer: issuer.into(),
            jwks,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The public JWKS served at `<issuer>/.well-known/jwks.json`.
    pub fn jwks(&self) -> &Value {
        &self.jwks
    }

    /// The OIDC discovery document served at
    /// `<issuer>/.well-known/openid-configuration`.
    pub fn discovery(&self) -> Value {
        let issuer = self.issuer.trim_end_matches('/');
        json!({
            "issuer": self.issuer,
            "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
            "id_token_signing_alg_values_supported": ["RS256"],
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
        })
    }

    /// Mint a short-lived RS256 JWT for an upstream call: `iss` = this issuer,
    /// `aud` = the intended audience(s), `sub` = the caller principal. `ttl` is
    /// clamped to [`MAX_TTL`].
    ///
    /// `audiences` is a slice so one token can name several intended recipients
    /// — e.g. the agent (`agents-<env>`) AND a downstream the agent forwards it
    /// to (`escurel-<env>`). Each verifier pins its own audience and matches if
    /// it appears in the array; this keeps every hop a *named* audience rather
    /// than replaying an agent-scoped token to a different service.
    ///
    /// `tenant`, when non-empty, is added as a `tenant` claim — a forwarded-to
    /// downstream (e.g. Escurel) may key its tenant off it. Empty → omitted.
    pub fn sign(
        &self,
        audiences: &[&str],
        subject: &str,
        tenant: &str,
        ttl: Duration,
    ) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("clock: {e}"))?
            .as_secs();
        let exp = now + ttl.min(MAX_TTL).as_secs();
        let mut claims = json!({
            "iss": self.issuer,
            "aud": audiences,
            "sub": subject,
            "iat": now,
            "exp": exp,
        });
        if !tenant.is_empty() {
            claims["tenant"] = json!(tenant);
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map_err(|e| format!("sign jwt: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::traits::PublicKeyParts;
    use rsa::{RsaPrivateKey, RsaPublicKey};
    use serde::Deserialize;

    /// Generate a throwaway 2048-bit RSA key + its matching JWKS, build a
    /// `JwtSigner`, then verify a minted token exactly as the reference agent
    /// does (RS256, issuer + audience pinned, kid-selected JWKS key). This is
    /// the agent-compatibility contract: if it passes, a real agent accepts
    /// Triton's static-mode token without a Vault mint.
    #[test]
    fn signs_a_token_the_oidc_verifier_accepts() {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
        let pem = private.to_pkcs8_pem(LineEnding::LF).expect("pkcs8 pem");
        let public = RsaPublicKey::from(&private);
        let b64 = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
        let kid = "test-key-1";
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

        let issuer = "https://triton.example.test";
        let signer = JwtSigner::from_rsa_pem(pem.as_bytes(), kid, issuer, jwks.clone())
            .expect("build signer");

        // Multi-audience: one token names both the agent and the downstream
        // (escurel) the agent forwards it to.
        let token = signer
            .sign(
                &["agents-nonprod", "escurel-nonprod"],
                "dz-triton-api",
                "default",
                Duration::from_secs(300),
            )
            .expect("sign");

        #[derive(Deserialize)]
        struct Claims {
            sub: String,
            tenant: String,
        }
        let set: JwkSet = serde_json::from_value(jwks).unwrap();
        let key = set.find(kid).expect("kid in jwks");
        let decoding = DecodingKey::from_jwk(key).expect("decoding key");
        assert_eq!(decode_header(&token).unwrap().kid.as_deref(), Some(kid));

        // Each hop pins ITS OWN audience and still accepts the multi-aud token:
        // the agent (verify_oidc, aud=agents-nonprod) and escurel (aud=escurel-nonprod).
        for aud in ["agents-nonprod", "escurel-nonprod"] {
            let mut validation = Validation::new(Algorithm::RS256);
            validation.set_issuer(&[issuer]);
            validation.set_audience(&[aud]);
            let data =
                decode::<Claims>(&token, &decoding, &validation).expect("verify for audience");
            assert_eq!(data.claims.sub, "dz-triton-api");
            assert_eq!(data.claims.tenant, "default");
        }

        // A verifier pinning a DIFFERENT audience must reject it.
        let mut wrong = Validation::new(Algorithm::RS256);
        wrong.set_issuer(&[issuer]);
        wrong.set_audience(&["someone-else"]);
        assert!(decode::<Claims>(&token, &decoding, &wrong).is_err());
    }

    #[test]
    fn discovery_points_at_jwks() {
        // No signing needed; a 512-bit key keeps this fast.
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 512).expect("keygen");
        let pem = private.to_pkcs8_pem(LineEnding::LF).expect("pem");
        let signer =
            JwtSigner::from_rsa_pem(pem.as_bytes(), "k", "https://t.test/", json!({"keys":[]}))
                .unwrap();
        let d = signer.discovery();
        assert_eq!(d["issuer"], "https://t.test/");
        assert_eq!(d["jwks_uri"], "https://t.test/.well-known/jwks.json");
    }
}
