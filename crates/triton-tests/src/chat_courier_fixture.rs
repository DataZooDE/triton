//! Fake `api.telegram.org` for the PR 18 outbound courier.
//!
//! The Telegram Bot API base is `https://api.telegram.org`; methods
//! sit under `/bot{token}/{method}` (the token is part of the path,
//! not a header). This fixture stands up a tiny axum server that
//! captures every `sendMessage` body it receives so a test can
//! assert on `chat_id` / `text` after the binary's courier fires.
//!
//! PR 36 adds a `sendPhoto` route — multipart/form-data carrying a
//! PNG file part — so dashboard rasterisation can be exercised
//! end-to-end against the real PNG bytes the rasterizer produced.
//!
//! No mocks per CLAUDE.md §1: this is a real HTTP server speaking
//! the Telegram Bot API wire shape over real TCP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{Multipart, Path, State};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// One captured `sendMessage` invocation. The token in the URL
/// path is asserted on so tests can confirm the adapter actually
/// used the resolved bot token (and not, e.g., a literal manifest
/// placeholder that survived a misconfigured Vault wiring).
#[derive(Debug, Clone)]
pub struct SentMessage {
    pub token: String,
    pub body: Value,
}

/// One captured `sendPhoto` multipart invocation (PR 36).
/// `photo_bytes` holds the raw photo file part — tests assert PNG
/// magic bytes against this to confirm the rasterizer actually
/// produced a PNG and the courier actually forwarded it.
#[derive(Debug, Clone)]
pub struct SentPhoto {
    pub token: String,
    pub chat_id: String,
    pub caption: Option<String>,
    pub parse_mode: Option<String>,
    pub reply_markup: Option<String>,
    pub photo_bytes: Vec<u8>,
}

/// Response profile the fake should return on each `sendMessage`.
#[derive(Debug, Clone)]
pub enum Profile {
    /// Default — `{ok: true, result: {message_id: 1}}`.
    Ok,
    /// `{ok: false, error_code, description, parameters: {retry_after}}`.
    /// Use for testing Codex PR 18 blocker 2 — 200-with-ok:false.
    Application {
        error_code: i64,
        retry_after: Option<u64>,
    },
}

struct FakeState {
    captured: Mutex<Vec<SentMessage>>,
    captured_photos: Mutex<Vec<SentPhoto>>,
    profile: Profile,
    /// Updates the `getUpdates` long-poll route serves. Each carries
    /// an `update_id`; the route returns those with `update_id >=
    /// offset`, mirroring real Telegram offset semantics.
    queued_updates: Mutex<Vec<Value>>,
}

pub struct FakeTelegramApi {
    addr: SocketAddr,
    state: Arc<FakeState>,
}

impl FakeTelegramApi {
    pub async fn start() -> Self {
        Self::with_profile(Profile::Ok).await
    }

    pub async fn with_profile(profile: Profile) -> Self {
        Self::build(profile, Vec::new()).await
    }

    /// Start a fake whose `getUpdates` route serves `updates` (each a
    /// full Telegram Update object with an `update_id`). Used by the
    /// long-poll inbound test.
    pub async fn with_updates(updates: Vec<Value>) -> Self {
        Self::build(Profile::Ok, updates).await
    }

    async fn build(profile: Profile, updates: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(FakeState {
            captured: Mutex::new(Vec::new()),
            captured_photos: Mutex::new(Vec::new()),
            profile,
            queued_updates: Mutex::new(updates),
        });

        let router = Router::new()
            .route("/bot{token}/sendMessage", post(handle_send_message))
            .route("/bot{token}/sendPhoto", post(handle_send_photo))
            .route("/bot{token}/getUpdates", post(handle_get_updates))
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> Vec<SentMessage> {
        self.state.captured.lock().unwrap().clone()
    }

    /// Captured `sendPhoto` multipart uploads (PR 36).
    pub fn captured_photos(&self) -> Vec<SentPhoto> {
        self.state.captured_photos.lock().unwrap().clone()
    }
}

/// Long-poll `getUpdates`: return queued updates with `update_id >=
/// offset`. Telegram treats a request with `offset = N` as an ack of
/// all updates `< N`, so once the worker advances its offset past the
/// seeded update, subsequent polls return an empty array.
async fn handle_get_updates(
    State(state): State<Arc<FakeState>>,
    Path(_token): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let offset = body.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let result: Vec<Value> = state
        .queued_updates
        .lock()
        .unwrap()
        .iter()
        .filter(|u| u.get("update_id").and_then(Value::as_i64).unwrap_or(0) >= offset)
        .cloned()
        .collect();
    Json(json!({ "ok": true, "result": result }))
}

async fn handle_send_photo(
    State(state): State<Arc<FakeState>>,
    Path(token): Path<String>,
    mut multipart: Multipart,
) -> Json<Value> {
    let mut chat_id = String::new();
    let mut caption: Option<String> = None;
    let mut parse_mode: Option<String> = None;
    let mut reply_markup: Option<String> = None;
    let mut photo_bytes: Vec<u8> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "photo" => {
                photo_bytes = field.bytes().await.unwrap_or_default().to_vec();
            }
            "chat_id" => {
                chat_id = field.text().await.unwrap_or_default();
            }
            "caption" => {
                caption = Some(field.text().await.unwrap_or_default());
            }
            "parse_mode" => {
                parse_mode = Some(field.text().await.unwrap_or_default());
            }
            "reply_markup" => {
                reply_markup = Some(field.text().await.unwrap_or_default());
            }
            _ => {
                // Drain and ignore unknown fields rather than
                // erroring — keeps the fixture forward-compatible
                // with any future telegram form fields the adapter
                // might add.
                let _ = field.bytes().await;
            }
        }
    }
    state.captured_photos.lock().unwrap().push(SentPhoto {
        token,
        chat_id,
        caption,
        parse_mode,
        reply_markup,
        photo_bytes,
    });
    match &state.profile {
        Profile::Ok => Json(json!({ "ok": true, "result": { "message_id": 1 } })),
        Profile::Application {
            error_code,
            retry_after,
        } => {
            let mut params = serde_json::Map::new();
            if let Some(s) = retry_after {
                params.insert("retry_after".to_string(), json!(s));
            }
            Json(json!({
                "ok": false,
                "error_code": error_code,
                "description": "fake telegram application error",
                "parameters": params,
            }))
        }
    }
}

async fn handle_send_message(
    State(state): State<Arc<FakeState>>,
    Path(token): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state
        .captured
        .lock()
        .unwrap()
        .push(SentMessage { token, body });
    match &state.profile {
        Profile::Ok => Json(json!({ "ok": true, "result": { "message_id": 1 } })),
        Profile::Application {
            error_code,
            retry_after,
        } => {
            let mut params = serde_json::Map::new();
            if let Some(s) = retry_after {
                params.insert("retry_after".to_string(), json!(s));
            }
            Json(json!({
                "ok": false,
                "error_code": error_code,
                "description": "fake telegram application error",
                "parameters": params,
            }))
        }
    }
}

// --- PR 33: Fake Google Chat JWKS fixture --------------------------

/// Pre-generated RSA-2048 test keypair. The cert is the public side
/// (PEM-wrapped X.509) and gets served via the fixture in the
/// canonical Google-cert-map shape. The private key is held by the
/// fixture so tests can mint JWTs the adapter will verify. Generated
/// once and embedded; tests run deterministically.
const FAKE_GOOGLE_TEST_PRIVATE_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDS0xMmgCRw+ELa
E1q5L1vsVCveCHbJgHixUncVJseEITGMZbhg2OmTVu3kvxSVcnlRArH4UGhNRKZD
gMWh7JuRygBtPgwPm37/91HaVHBfrbSnPnuH768iwC6r9GtHr8RhGGxNsTXkKYXp
Kere8mV9My3zjPDt2ZAmF9eCr5KF6cWwdgh8EVI5rirXj+RQ+8xISKc5qy9FdJ3z
GPNaYVEE2U/ps3I/DB8m5m2SgMNzvXnIyQlVE+wE4BnUnppPwdKcwO2PoZU1JtWu
Wjm9omdiCIsgBBSI2VaUy5cKv/uAOBgxOiJ6560kr6tKoQb0/qWRt+GCG1+tS9lL
TzeqQyTbAgMBAAECggEADGaI86n0Osbc28npULIwmdEw9rGQbHVyOUU3UZcDLkpA
3q1kWndNHzMCRthXGXFEdyzi6KRbdja0VuJzUtJsK3edGHqJUr5b39TIIOui49B/
q0SuwcX/Na/l2YxvZlCNiwPY6aWjnK+KkQvmT5skuBKzaxDJDRx6cPB/EdCfnDZo
EwY8ihTGffg2fBGZdmJ6FcI222lp79H1P061ffEcKkDe3S2n4QeIPIzd4jMyu2uz
y9famGb6ee1xA/WQm56SWsRuu9Mow44AvuI9B6/vV1Q/Wy/mxm4/KADLTuf89B3+
zrE9GIKiSYglWJvZuJ0TtPX/08v27N/S9vPZuOJUTQKBgQDvV5RP7+c76Aa0L9bD
G3mef7inxCe1hS0vCzSXQ1/3PH4WrVbyTnGXyx0hLVdHEf5vRco0HOcYk9HWnoe2
/6w/bhoy4y85zmiTuim2HP/zK47+bxXVtgFa4FvHsrPiLjWtmim3+E6rvsncdZJ0
Kdg3hR1ZqmyjcX16ocepvpkMbQKBgQDhf2PY6ZUirVeFEdGfNlTjnvKezUGvLG9X
W1wqdd2JmI+RsNq92uHhPTsV4+DTnV1hsbGlcUWYC0P4UX1ECbuinHPTZPDwnyyR
KqLEeIlIWn8VH8CGR+8y6IREBjirXxLQSOV2eDYcsVJoXdwc7dRhihrnh+pphPk5
3/FYXgqZZwKBgHsIYTQqVYqE/pU3lkWbZQxmCW0sN2FnQU/SiclMGBPGo+ZSWsSa
MGhgP+wjG59sD4fxrzzUsrL+obqaqZcXnNrKZWtNP6SOh1GRPAnipGvDM3F1dxrx
wYaOmH9yTGfzayJ/gfyRBxfgLnJGee9+5ye7JNhH9Cqcl20nprSKRrCNAoGATDaI
Apn/w7aea+U32f289ymTisSIvLHh975zCg7ID2c2ruD9LUm7KitNuvpH1H3NP+WU
yvvbr6WvFVBFbCd1+WGza/Ej1c+WeoHUfV7X11JuvS78HOZXG/emLG+F27XIYAkj
NMUwVMZBufBvIn/nVggdS7+OJJfCvCLKKTmvj2UCgYEA1ZMVlxzMw3Nfw1y81oH/
o5oqRmEL+rP2cnjVbrM24FXp1N/AcraViT6psze9IzaQRHVrLenrlpkxduwyWcqw
h1YW3vzGXFhN+nmS4yLQv/IkKbgL+5W3s1jAIGQ2nGPavYtXNQhxPwR/Puid2Epa
fXAa07/tRgy568bgfRvl1ek=
-----END PRIVATE KEY-----
";

const FAKE_GOOGLE_TEST_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIDJzCCAg+gAwIBAgIUPreZqvWH6ubjESrbdpEVQNtMIc4wDQYJKoZIhvcNAQEL
BQAwIjEgMB4GA1UEAwwXdHJpdG9uLXRlc3QtZ29vZ2xlLWNoYXQwIBcNMjYwNTI1
MDU1NDM1WhgPMjEyNjA1MDEwNTU0MzVaMCIxIDAeBgNVBAMMF3RyaXRvbi10ZXN0
LWdvb2dsZS1jaGF0MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA0tMT
JoAkcPhC2hNauS9b7FQr3gh2yYB4sVJ3FSbHhCExjGW4YNjpk1bt5L8UlXJ5UQKx
+FBoTUSmQ4DFoeybkcoAbT4MD5t+//dR2lRwX620pz57h++vIsAuq/RrR6/EYRhs
TbE15CmF6Snq3vJlfTMt84zw7dmQJhfXgq+ShenFsHYIfBFSOa4q14/kUPvMSEin
OasvRXSd8xjzWmFRBNlP6bNyPwwfJuZtkoDDc715yMkJVRPsBOAZ1J6aT8HSnMDt
j6GVNSbVrlo5vaJnYgiLIAQUiNlWlMuXCr/7gDgYMToieuetJK+rSqEG9P6lkbfh
ghtfrUvZS083qkMk2wIDAQABo1MwUTAdBgNVHQ4EFgQUIn8b+T0lvHl4npwmEspR
LZGvESkwHwYDVR0jBBgwFoAUIn8b+T0lvHl4npwmEspRLZGvESkwDwYDVR0TAQH/
BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAymw9i75xSCsG3fOv0ggchTn26DOd
o8USAK4/lyTfPCiIWI7T8K16QQY6g6QRnixR+NG/j853BniWve2EL9cIpW4JFSd0
cM3J8mF1vOAD3JSQFehBAchBMQPlZjsgGy/BUDdNZQ2K7T+y9OrhfH5aRJi3RoGW
WmC5ca+xHYaWzvyEo0bj80s9sUch7fLVeOF3rvnpZh7Oleg1S7sJmtnT6ZUfic08
d2R0Srw0+3zqL7erChYucW8+J+3KzuBPNOh/V91KjDscCbdvw15jCcxLAe9Z3R6Z
z2uBnXHKlSXLvqnvPtyPBnTvX4NS2ZMv7fMOH2cwY2NNfWkUTUslMqTkBw==
-----END CERTIFICATE-----
";

/// Fake Google Chat JWKS (cert-map) endpoint. Serves the canonical
/// PEM-wrapped X.509 cert under the chosen `kid`; gives the test
/// access to a matching JWT signer.
///
/// Per CLAUDE.md §1 this is a real axum HTTP server, not a mock —
/// the binary fetches over real TCP, the adapter parses the real
/// PEM, and the test mints real RS256-signed JWTs.
pub struct FakeGoogleJwks {
    addr: SocketAddr,
    kid: String,
    signing_key: jsonwebtoken::EncodingKey,
}

impl FakeGoogleJwks {
    pub async fn start() -> Self {
        Self::start_with_kid("triton-test-google-chat-key").await
    }

    /// Same as `start()` but lets the test pick the `kid` so a
    /// negative test can exercise the "kid in header doesn't match
    /// any served cert" path.
    pub async fn start_with_kid(kid: &str) -> Self {
        let signing_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(FAKE_GOOGLE_TEST_PRIVATE_KEY_PEM.as_bytes())
                .expect("rsa private key parses");
        let kid_str = kid.to_string();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();

        let cert_map_body = serde_json::json!({
            &kid_str: FAKE_GOOGLE_TEST_CERT_PEM
        });
        // Realistic OIDC JWKS (`oauth2/v3/certs` shape) built from the SAME
        // key, so the #134 OIDC path verifies an `accounts.google.com`
        // token against a JWKS keyset exactly like production — not just
        // the legacy x509 cert-map.
        let jwks_body = jwks_from_pkcs8_pem(FAKE_GOOGLE_TEST_PRIVATE_KEY_PEM, &kid_str);
        let router = Router::new()
            .route(
                "/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com",
                axum::routing::get(move || {
                    let body = cert_map_body.clone();
                    async move { Json(body) }
                }),
            )
            .route(
                "/oauth2/v3/certs",
                axum::routing::get(move || {
                    let body = jwks_body.clone();
                    async move { Json(body) }
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            addr,
            kid: kid_str,
            signing_key,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Canonical path on Google's domain. The adapter is configured
    /// via `TRITON_GOOGLE_CHAT_JWKS_URI = <fake.url()><path>`; the
    /// path is the same shape real Google uses, so we exercise the
    /// adapter's URL handling unmodified.
    pub fn jwks_uri(&self) -> String {
        format!(
            "{}/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com",
            self.url()
        )
    }

    /// JWKS-shaped keyset endpoint (`oauth2/v3/certs`), the source the
    /// current Google Chat console's OIDC tokens verify against (#134).
    pub fn oidc_jwks_uri(&self) -> String {
        format!("{}/oauth2/v3/certs", self.url())
    }

    /// Sign a JWT using the fixture's private key, with `kid` set
    /// to the served cert's id (so the adapter's keyset lookup hits
    /// the right key).
    pub fn sign_jwt(&self, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.signing_key).expect("sign jwt")
    }

    /// Sign a JWT under a DIFFERENT kid than the one the fixture
    /// publishes — exercises the "unknown kid" rejection path.
    pub fn sign_jwt_with_kid(&self, kid: &str, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, &claims, &self.signing_key).expect("sign jwt")
    }
}

/// Build a JWKS document (`{"keys":[{kty,use,alg,kid,n,e}]}`) from a
/// PKCS#8 RSA private key PEM — the modulus/exponent are base64url
/// (no pad) per RFC 7518, matching what Google's `oauth2/v3/certs`
/// serves.
fn jwks_from_pkcs8_pem(pkcs8_pem: &str, kid: &str) -> Value {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::traits::PublicKeyParts;

    let key = RsaPrivateKey::from_pkcs8_pem(pkcs8_pem).expect("pkcs8 rsa key parses");
    let n = URL_SAFE_NO_PAD.encode(key.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(key.e().to_bytes_be());
    serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": kid,
            "n": n,
            "e": e,
        }]
    })
}

/// A second, ALWAYS-different RSA signer so the forged-signature
/// test can produce a JWT that verifies syntactically against any
/// RS256 key but won't match the fixture's public cert. Re-uses
/// the jsonwebtoken-shipped PKCS8 PEM from a separately generated
/// key.
pub fn attacker_signing_key() -> jsonwebtoken::EncodingKey {
    // Distinct keypair — generated alongside the fixture key but
    // never published.
    const ATTACKER_PRIVATE_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCXsNyW9t9swgdC
cBd5cdQleIjpdVb8/4MMBslsWuH2Zt3JMikVWwZgy4TACI3VUz9NNbt/ZEC7qKL4
FJwdR0Xf15ZJTXPu8ecfckPdsN9MH5pOOoZVq6RJZzVhLtO4nBEct7kT4IKZnUIj
Fm1ni+U08oR3/q2xGGTCEia3vr0B4FbaPiQOY9D4a0CLphfH+XFDd8yARP3eXkKa
NNMNHfwKMF7nzrgDj730JQw0gJN0jWGwRxygJmgq7NXP6aZ5xVCSXwhGgiQ3vLS6
5mi/SJN9GFxqLGCJjuKb+PmkENGoLTpbJk/NjVc4+JAhjgImyke7+58YbHIOj2tu
9vazHMr7AgMBAAECggEARuEmgggDKE+Vks7LuTyeE5A58VSZ/AfslQ8KyW3CDh/M
3HlqxwbMeSg/9HdKxvZqKsrDvOf8c3N+CwueUvP9y3VyTPg7BtjT1VbQLWO7Q1e1
A37HTHqyfnYSdEGsPqP2PwP+IDKU8/COedS99FdjF5WGnodLY+fxFNnka5FdweT4
R7FQijtIrT/o8bdoFFmLDwa/m9nUiZ01pkkXx9+yszei1NMaQ+Zf86ixuWA3f6X2
z//gUr1mz6N+euF6ebdL4fr8gowoep17hTvZsZyB+43YMdKs1BMbQg+BLXmInFj8
nJ908TjnbX8XHhGBcltVDMKatYawBVUuS1xk+b97gQKBgQDSAt6j7wqUwCsZX116
ewy0sIJvcengqJi//xBl3h/rf8B61d6z+HJ3ZMkUoZTSXN3g9qzoN4ZdhqVYUDYl
j38T5CfnkD1GYYwfgDqIKwXgkQVLXob6Ujp8CQ4OujyKfrZ3rb8pAJl1J7FwBXY3
Oo9vOTJvxTMbrz+mRFP/pSHXOwKBgQC46JSsvfv5kCo7cKnObsJ90jfX1roCsHPi
MgVWC0raL793fXPdXOixtNSci1KJxv5bNxfGBYxGSb2nbZgn6nOqyuYPM8bCgrfb
41NqSyDA4eTOUic0JtyPqRs5/P375Fc2D8Vz9ImKuMCYdIAPLDm2uns1sbGnh0T4
956oMA8fQQKBgQC+6nDv4s1hsNj9dd6LC/XfBV9uZMZSv7ItSHjlwmqOMlMO2AJe
5YtZ0ruiD8o0+suSSW2ipWd2+oKxqCmxN6Q0twM31b5+jwtNT8rmIwZywiNoAwT9
52bXf3vSE6gZ11uVrNPNOIhJIs6BodV4G7ptSDf7t+/gSQ653f/mtX3wJQKBgAIn
0/PfkxxprdRbj980M1g8JyKBAlIdtHwikSVbpFe+zsCZ2cvu1VedAA2DIkcw5q4x
ijlovyXini9he7CbbxXCn8P1mo+R7orFr6dBkPQurfgpxQM6oL+b/RFD/cH9+3ZJ
4MdlRmUzmiss0IFcxp92tRD/LU8CqK8uU88qIEMBAoGAIer57oeUNXKLTpD3O0YW
BusmQA9+y0shzyNFd7uG6D11gjQmTvA2bnVAKkpuLgocqoMopZPdPPNo4Hsa0J1S
h1LTVvd4efVbB+222bZJsT2d3+xmC6gGJHgIcbeV7tPB1KBDq0ekaIioPDEwZkqm
AxyCpZKaHijbEp5kx+XFVIQ=
-----END PRIVATE KEY-----
";
    jsonwebtoken::EncodingKey::from_rsa_pem(ATTACKER_PRIVATE_KEY_PEM.as_bytes())
        .expect("attacker rsa private key parses")
}

// --- PR 35: Fake Bot Framework fixture ----------------------------

/// JWK `n` (base64url-no-pad) for [`FAKE_GOOGLE_TEST_CERT_PEM`].
/// Re-derived once at fixture authoring time so the JWKS endpoint
/// can be served without pulling in an RSA / X.509 parser. (Same
/// keypair as the Google fixture above — both adapters need an
/// RSA-2048 signer, so we share the cert.)
const FAKE_RSA_N_B64URL: &str = "0tMTJoAkcPhC2hNauS9b7FQr3gh2yYB4sVJ3FSbHhCExjGW4YNjpk1bt5L8UlXJ5UQKx-FBoTUSmQ4DFoeybkcoAbT4MD5t-__dR2lRwX620pz57h--vIsAuq_RrR6_EYRhsTbE15CmF6Snq3vJlfTMt84zw7dmQJhfXgq-ShenFsHYIfBFSOa4q14_kUPvMSEinOasvRXSd8xjzWmFRBNlP6bNyPwwfJuZtkoDDc715yMkJVRPsBOAZ1J6aT8HSnMDtj6GVNSbVrlo5vaJnYgiLIAQUiNlWlMuXCr_7gDgYMToieuetJK-rSqEG9P6lkbfhghtfrUvZS083qkMk2w";
const FAKE_RSA_E_B64URL: &str = "AQAB";

/// One captured Activity reply on the conversation endpoint. The
/// fixture stores both the bearer presented and the JSON body so
/// integration tests can assert on both the access-token path and
/// the rendered text.
#[derive(Debug, Clone)]
pub struct CapturedActivity {
    pub conversation_id: String,
    pub bearer: String,
    pub body: Value,
}

/// A tiny axum app that pretends to be the Microsoft Bot Framework:
/// serves OpenID discovery + JWKS, mints stub OAuth2 access tokens
/// on the client_credentials endpoint, and captures reply
/// Activities POSTed at `<base>/v3/conversations/{id}/activities`.
///
/// JWT signing key is the same RSA-2048 keypair the Google fixture
/// uses (PEM constants above). The test calls `sign_jwt` to mint
/// inbound Bot Framework JWTs the adapter will verify.
pub struct FakeBotFramework {
    addr: SocketAddr,
    kid: String,
    signing_key: jsonwebtoken::EncodingKey,
    state: Arc<FakeBotFrameworkState>,
}

struct FakeBotFrameworkState {
    captured: Mutex<Vec<CapturedActivity>>,
    access_token: String,
}

impl FakeBotFramework {
    pub async fn start() -> Self {
        Self::with_access_token("fake-bot-access-token").await
    }

    pub async fn with_access_token(access_token: &str) -> Self {
        let signing_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(FAKE_GOOGLE_TEST_PRIVATE_KEY_PEM.as_bytes())
                .expect("rsa private key parses");
        let kid = "triton-test-msteams-key".to_string();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let jwks_uri = format!("{base_url}/v1/.well-known/keys");

        let state = Arc::new(FakeBotFrameworkState {
            captured: Mutex::new(Vec::new()),
            access_token: access_token.to_string(),
        });

        let discovery_body = json!({
            "issuer": "https://api.botframework.com",
            "jwks_uri": jwks_uri,
            "id_token_signing_alg_values_supported": ["RS256"],
        });
        let jwks_body = json!({
            "keys": [{
                "kty": "RSA",
                "kid": kid.clone(),
                "use": "sig",
                "alg": "RS256",
                "n": FAKE_RSA_N_B64URL,
                "e": FAKE_RSA_E_B64URL,
            }]
        });

        let discovery = discovery_body.clone();
        let jwks = jwks_body.clone();
        let token_state = state.clone();
        let activities_state = state.clone();

        let router = Router::new()
            .route(
                "/v1/.well-known/openidconfiguration",
                axum::routing::get(move || {
                    let d = discovery.clone();
                    async move { Json(d) }
                }),
            )
            .route(
                "/v1/.well-known/keys",
                axum::routing::get(move || {
                    let j = jwks.clone();
                    async move { Json(j) }
                }),
            )
            .route(
                "/oauth2/v2.0/token",
                post(move || {
                    let s = token_state.clone();
                    async move {
                        Json(json!({
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "ext_expires_in": 3600,
                            "access_token": s.access_token.clone(),
                        }))
                    }
                }),
            )
            .route(
                "/v3/conversations/{conversation_id}/activities",
                post(handle_activity_post).with_state(activities_state),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            addr,
            kid,
            signing_key,
            state,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// OpenID discovery URL the adapter is configured against.
    pub fn openid_url(&self) -> String {
        format!("{}/v1/.well-known/openidconfiguration", self.url())
    }

    /// OAuth2 token URL the token client is configured against.
    pub fn token_url(&self) -> String {
        format!("{}/oauth2/v2.0/token", self.url())
    }

    /// Bot's `serviceUrl` claim — captures reply Activities. Trailing
    /// slash mirrors Microsoft's documented shape.
    pub fn service_url(&self) -> String {
        format!("{}/", self.url())
    }

    /// Sign a JWT under the fixture's RSA key with `kid` set to
    /// what the JWKS endpoint publishes. `claims` is whatever the
    /// test wants in the payload; Bot Framework validation expects
    /// `iss = https://api.botframework.com`, `aud = <appid>`, an
    /// unexpired `exp`, and a non-empty `serviceUrl`.
    pub fn sign_jwt(&self, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.signing_key).expect("sign jwt")
    }

    /// Snapshot of every reply Activity the fixture captured.
    pub fn captured(&self) -> Vec<CapturedActivity> {
        self.state.captured.lock().unwrap().clone()
    }
}

async fn handle_activity_post(
    axum::extract::State(state): axum::extract::State<Arc<FakeBotFrameworkState>>,
    axum::extract::Path(conversation_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.captured.lock().unwrap().push(CapturedActivity {
        conversation_id,
        bearer,
        body,
    });
    Json(json!({ "id": "stub-activity-id" }))
}

// ---------- Google Chat REST API fake (#164 T1a) ----------

/// One captured create-message POST against the fake Google Chat
/// REST API (#164 T1a async reply courier). `space` is the parent
/// resource reconstructed from the URL path (`spaces/<id>`);
/// `bearer` is the verbatim `Authorization` header value so tests
/// can pin the courier's credential; `body` is the posted Message
/// JSON (`{"text": …}` for T1a).
#[derive(Debug, Clone)]
pub struct GoogleChatSentMessage {
    pub space: String,
    pub bearer: String,
    pub body: Value,
}

struct GoogleChatApiState {
    captured: Mutex<Vec<GoogleChatSentMessage>>,
    /// HTTP status every POST answers with — 200 for the happy path,
    /// 500 to exercise the courier's Retry audit branch.
    status: u16,
}

/// Fake `chat.googleapis.com` for the #164 T1a async reply courier.
/// Speaks the `POST /v1/spaces/{space}/messages` wire shape with a
/// stub `{name: "spaces/<id>/messages/stub"}` response.
///
/// No mocks per CLAUDE.md §1: a real axum HTTP server on a real TCP
/// port; the spawned courier task inside the binary POSTs to it over
/// real HTTP.
pub struct FakeGoogleChatApi {
    addr: SocketAddr,
    state: Arc<GoogleChatApiState>,
}

impl FakeGoogleChatApi {
    pub async fn start() -> Self {
        Self::start_with_status(200).await
    }

    /// Same as [`start`](Self::start) but every POST answers `status`
    /// (still capturing the request), so tests can exercise the
    /// courier's non-2xx `record_post` branches.
    pub async fn start_with_status(status: u16) -> Self {
        let state = Arc::new(GoogleChatApiState {
            captured: Mutex::new(Vec::new()),
            status,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/v1/spaces/{space}/messages",
            post(handle_chat_message_post).with_state(state.clone()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Snapshot of every Message POST the fixture captured.
    pub fn captured(&self) -> Vec<GoogleChatSentMessage> {
        self.state.captured.lock().unwrap().clone()
    }
}

async fn handle_chat_message_post(
    State(state): State<Arc<GoogleChatApiState>>,
    Path(space): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.captured.lock().unwrap().push(GoogleChatSentMessage {
        space: format!("spaces/{space}"),
        bearer,
        body,
    });
    let status =
        axum::http::StatusCode::from_u16(state.status).unwrap_or(axum::http::StatusCode::OK);
    (
        status,
        Json(json!({ "name": format!("spaces/{space}/messages/stub") })),
    )
        .into_response()
}

// ---------- WhatsApp Cloud API fake (PR 31) ----------

/// One captured `messages` POST against the fake WhatsApp Cloud
/// API. `phone_number_id` is the URL-path segment; `authorization`
/// is the verbatim `Authorization` header value so tests can assert
/// the bearer token actually made it through credential resolution.
#[derive(Debug, Clone)]
pub struct WhatsAppSentMessage {
    pub phone_number_id: String,
    pub authorization: String,
    pub body: Value,
}

/// One captured `/media` multipart upload (PR 38). `photo_bytes`
/// holds the verbatim PNG body so tests can assert PNG magic + size
/// without re-uploading anything.
#[derive(Debug, Clone)]
pub struct WhatsAppCapturedMedia {
    pub phone_number_id: String,
    pub authorization: String,
    pub messaging_product: String,
    pub kind: String,
    pub photo_bytes: Vec<u8>,
}

struct WhatsAppState {
    captured: Mutex<Vec<WhatsAppSentMessage>>,
    captured_media: Mutex<Vec<WhatsAppCapturedMedia>>,
}

/// Fake `graph.facebook.com` for the PR 31 outbound courier + PR 38
/// media upload. Speaks the `/v18.0/{phone_number_id}/messages` wire
/// shape with a stub
/// `{messaging_product, contacts, messages: [{id: "wamid.stub"}]}`
/// response, and the `/v18.0/{phone_number_id}/media` wire shape
/// with a stub `{id: "media_id_stub"}` response.
pub struct FakeWhatsAppApi {
    addr: SocketAddr,
    state: Arc<WhatsAppState>,
}

impl FakeWhatsAppApi {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 0");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(WhatsAppState {
            captured: Mutex::new(Vec::new()),
            captured_media: Mutex::new(Vec::new()),
        });

        let router = Router::new()
            .route(
                "/v18.0/{phone_number_id}/messages",
                post(handle_whatsapp_send),
            )
            .route(
                "/v18.0/{phone_number_id}/media",
                post(handle_whatsapp_media),
            )
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn captured(&self) -> Vec<WhatsAppSentMessage> {
        self.state.captured.lock().unwrap().clone()
    }

    /// PR 38: captured `/v18.0/{id}/media` multipart uploads. Each
    /// entry carries the raw PNG bytes so tests can assert PNG magic
    /// without re-extracting them from the form.
    pub fn captured_media(&self) -> Vec<WhatsAppCapturedMedia> {
        self.state.captured_media.lock().unwrap().clone()
    }
}

async fn handle_whatsapp_send(
    State(state): State<Arc<WhatsAppState>>,
    Path(phone_number_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.captured.lock().unwrap().push(WhatsAppSentMessage {
        phone_number_id: phone_number_id.clone(),
        authorization,
        body,
    });
    Json(json!({
        "messaging_product": "whatsapp",
        "contacts": [{ "input": "stub", "wa_id": "stub" }],
        "messages": [{ "id": "wamid.STUB" }],
    }))
}

/// PR 38: WhatsApp Cloud `/v18.0/{id}/media` multipart upload route.
/// Captures the PNG file part + the `messaging_product` and `type`
/// text parts, then returns a stub `{id: "media_id_stub"}` envelope
/// that the adapter plugs into the subsequent image-message body.
async fn handle_whatsapp_media(
    State(state): State<Arc<WhatsAppState>>,
    Path(phone_number_id): Path<String>,
    headers: axum::http::HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Json<Value> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut messaging_product = String::new();
    let mut kind = String::new();
    let mut photo_bytes: Vec<u8> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                photo_bytes = field.bytes().await.unwrap_or_default().to_vec();
            }
            "messaging_product" => {
                messaging_product = field.text().await.unwrap_or_default();
            }
            "type" => {
                kind = field.text().await.unwrap_or_default();
            }
            _ => {
                // Forward-compatible: drain unknown fields so the
                // fixture doesn't reject future shape additions.
                let _ = field.bytes().await;
            }
        }
    }
    state
        .captured_media
        .lock()
        .unwrap()
        .push(WhatsAppCapturedMedia {
            phone_number_id,
            authorization,
            messaging_product,
            kind,
            photo_bytes,
        });
    Json(json!({ "id": "media_id_stub" }))
}
