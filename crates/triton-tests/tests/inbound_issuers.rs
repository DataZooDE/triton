//! Several inbound issuers in the `triton` binary.
//!
//! One deployment serves callers from different identity providers at
//! once: Gemini Enterprise forwards opaque **Google access tokens**,
//! Copilot Studio and MCP clients bring **Entra** tokens from any
//! allow-listed customer tenant, and the deployment's own OIDC issuer
//! keeps working. Whichever path verified the caller, the resolved
//! principal names the identity provider (`idp`), the provider's tenant
//! (`idp_tenant`: Google `hd`, Entra `tid`) and the verified `email`, and
//! with `TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL=true` the upstream JWT
//! carries those three claims.
//!
//! No mocks: two real `TestIssuer`s (one plays the Entra authority at a
//! local URL), a fake Google tokeninfo endpoint that speaks Google's wire
//! shape, a real `FakeAgent` upstream, and the real binary.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const SIGNING_KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_key.pem"
));
const SIGNER_JWKS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_jwks.json"
));

const OIDC_AUD: &str = "triton-trio";
const ENTRA_AUD: &str = "api://triton-copilot";
const TID_ACME: &str = "11111111-2222-3333-4444-555555555555";
const TID_OTHER: &str = "99999999-8888-7777-6666-555555555555";
pub(crate) const GOOGLE_CLIENT: &str = "1234-gemini.apps.googleusercontent.com";
pub(crate) const GOOGLE_TOKEN: &str = "ya29.opaque-google-access-token";
const GOOGLE_TOKEN_OUTSIDE: &str = "ya29.opaque-token-from-another-domain";
/// A consumer Google account whose email is on the customer's domain but
/// that is no Workspace member: no `hd`.
const GOOGLE_TOKEN_NO_HD: &str = "ya29.opaque-consumer-account";

/// Google's tokeninfo endpoint, as far as Triton consumes it: `GET
/// ?access_token=<t>` answers the token's audience, verified email,
/// hosted domain and expiry, or `400` for an unknown token.
pub(crate) struct FakeTokeninfo {
    addr: SocketAddr,
    hits: Arc<Mutex<u32>>,
}

impl FakeTokeninfo {
    pub(crate) async fn start() -> Self {
        let hits = Arc::new(Mutex::new(0u32));
        let app = Router::new()
            .route("/tokeninfo", get(tokeninfo))
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, hits }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/tokeninfo", self.addr)
    }
}

async fn tokeninfo(
    State(hits): State<Arc<Mutex<u32>>>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    *hits.lock().unwrap() += 1;
    let exp = (now() + 3600).to_string();
    match q.get("access_token").map(String::as_str) {
        Some(GOOGLE_TOKEN) => Json(json!({
            "aud": GOOGLE_CLIENT,
            "azp": GOOGLE_CLIENT,
            "email": "alice@acme.example",
            "email_verified": "true",
            "hd": "acme.example",
            "scope": "openid email https://www.googleapis.com/auth/userinfo.email",
            "exp": exp,
        }))
        .into_response(),
        Some(GOOGLE_TOKEN_NO_HD) => Json(json!({
            "aud": GOOGLE_CLIENT,
            "email": "eve@acme.example",
            "email_verified": "true",
            "exp": exp,
        }))
        .into_response(),
        Some(GOOGLE_TOKEN_OUTSIDE) => Json(json!({
            "aud": GOOGLE_CLIENT,
            "email": "mallory@elsewhere.example",
            "email_verified": "true",
            "hd": "elsewhere.example",
            "exp": exp,
        }))
        .into_response(),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_token" })),
        )
            .into_response(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn entra_issuer(authority: &str, tid: &str) -> String {
    format!("{authority}/{tid}/v2.0")
}

struct Rig {
    triton: TritonProcess,
    agent: FakeAgent,
    oidc: TestIssuer,
    entra: TestIssuer,
    _google: FakeTokeninfo,
}

async fn rig() -> Rig {
    let agent = FakeAgent::start_echoing().await;
    let oidc = TestIssuer::start().await;
    // The Entra authority is a second local issuer: tokens carry
    // `iss = <authority>/<tid>/v2.0`, keys come from its JWKS.
    let entra = TestIssuer::start().await;
    let google = FakeTokeninfo::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_OIDC_ISSUER".to_string(), oidc.issuer_url()),
        ("TRITON_OIDC_AUDIENCE".to_string(), OIDC_AUD.to_string()),
        ("TRITON_ENTRA_AUDIENCE".to_string(), ENTRA_AUD.to_string()),
        (
            "TRITON_ENTRA_ALLOWED_TENANTS".to_string(),
            format!("{TID_ACME}=acme"),
        ),
        ("TRITON_ENTRA_AUTHORITY".to_string(), entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL".to_string(), entra.jwks_url()),
        (
            "TRITON_GOOGLE_ACCESS_AUDIENCES".to_string(),
            GOOGLE_CLIENT.to_string(),
        ),
        (
            "TRITON_GOOGLE_ACCESS_ALLOWED_HD".to_string(),
            "acme.example=acme".to_string(),
        ),
        ("TRITON_GOOGLE_TOKENINFO_URL".to_string(), google.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), SIGNER_JWKS.to_string()),
        (
            "TRITON_JWT_KID".to_string(),
            "triton-test-signer".to_string(),
        ),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL".to_string(),
            "true".to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    Rig {
        triton,
        agent,
        oidc,
        entra,
        _google: google,
    }
}

async fn call(triton: &TritonProcess, token: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth(token)
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant")
        .status()
}

fn jwt_claims(jwt: &str) -> Value {
    let payload = jwt.split('.').nth(1).expect("payload segment");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("b64")).expect("json")
}

fn last_upstream_claims(agent: &FakeAgent) -> Value {
    let bearer = agent.bearers_seen().pop().expect("the upstream was called");
    jwt_claims(&bearer)
}

fn entra_token(entra: &TestIssuer, tid: &str, extra: Value) -> String {
    let mut claims = json!({
        "iss": entra_issuer(&entra.issuer_url(), tid),
        "aud": ENTRA_AUD,
        "sub": "entra-pairwise-sub",
        "oid": "00000000-0000-0000-0000-0000000000aa",
        "tid": tid,
        "exp": now() + 600,
        "iat": now(),
        "scp": "access_as_user",
    });
    if let (Some(c), Value::Object(e)) = (claims.as_object_mut(), extra) {
        c.extend(e);
    }
    entra.sign_jwt(claims)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entra_caller_from_an_allowed_tenant_reaches_the_upstream_with_idp_claims() {
    let r = rig().await;
    let token = entra_token(
        &r.entra,
        TID_ACME,
        json!({ "preferred_username": "Bob@Acme.Example" }),
    );
    assert_eq!(call(&r.triton, &token).await, reqwest::StatusCode::OK);

    let claims = last_upstream_claims(&r.agent);
    assert_eq!(claims["sub"], "entra-pairwise-sub");
    assert_eq!(claims["tenant"], "acme", "the tid maps to the data tenant");
    assert_eq!(claims["idp"], "entra");
    assert_eq!(claims["idp_tenant"], TID_ACME);
    assert_eq!(
        claims["email"], "bob@acme.example",
        "Entra access tokens carry the sign-in name, not `email`; it is lower-cased"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entra_tenant_outside_the_allow_list_is_refused() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_OTHER, json!({}));
    assert_eq!(
        call(&r.triton, &token).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert!(r.agent.bearers_seen().is_empty(), "no dispatch happened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_google_access_token_is_verified_by_tokeninfo_with_the_hd_restriction() {
    let r = rig().await;
    assert_eq!(call(&r.triton, GOOGLE_TOKEN).await, reqwest::StatusCode::OK);
    let claims = last_upstream_claims(&r.agent);
    assert_eq!(claims["sub"], "alice@acme.example");
    assert_eq!(claims["email"], "alice@acme.example");
    assert_eq!(claims["idp"], "google");
    assert_eq!(claims["idp_tenant"], "acme.example");
    assert_eq!(claims["tenant"], "acme", "the hd maps to the data tenant");

    assert_eq!(
        call(&r.triton, GOOGLE_TOKEN_OUTSIDE).await,
        reqwest::StatusCode::UNAUTHORIZED,
        "a verified Google user outside the allowed hosted domains is refused"
    );
    assert_eq!(
        call(&r.triton, "ya29.unknown").await,
        reqwest::StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_deployments_own_issuer_still_works_beside_them() {
    let r = rig().await;
    let token = r.oidc.sign_jwt(json!({
        "iss": r.oidc.issuer_url(),
        "aud": OIDC_AUD,
        "sub": "carol",
        "tenant": "globex",
        "email": "Carol@Globex.Example",
        "exp": now() + 600,
    }));
    assert_eq!(call(&r.triton, &token).await, reqwest::StatusCode::OK);
    let claims = last_upstream_claims(&r.agent);
    assert_eq!(claims["sub"], "carol");
    assert_eq!(claims["tenant"], "globex");
    assert_eq!(claims["idp"], "oidc");
    assert_eq!(claims["email"], "carol@globex.example");
    assert!(
        claims.get("idp_tenant").is_none(),
        "a generic issuer has no provider tenant"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_forwarding_the_idp_claims_stay_out_of_the_upstream_token() {
    let agent = FakeAgent::start_echoing().await;
    let entra = TestIssuer::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_ENTRA_AUDIENCE".to_string(), ENTRA_AUD.to_string()),
        (
            "TRITON_ENTRA_ALLOWED_TENANTS".to_string(),
            TID_ACME.to_string(),
        ),
        ("TRITON_ENTRA_AUTHORITY".to_string(), entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL".to_string(), entra.jwks_url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), SIGNER_JWKS.to_string()),
        (
            "TRITON_JWT_KID".to_string(),
            "triton-test-signer".to_string(),
        ),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let token = entra_token(&entra, TID_ACME, json!({ "email": "bob@acme.example" }));
    assert_eq!(call(&triton, &token).await, reqwest::StatusCode::OK);
    let claims = last_upstream_claims(&agent);
    assert_eq!(
        claims["tenant"], TID_ACME,
        "a bare tid in the allow-list is its own tenant"
    );
    for absent in ["email", "idp", "idp_tenant"] {
        assert!(
            claims.get(absent).is_none(),
            "{absent} is forwarded only on opt-in"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overriding_a_provider_endpoint_outside_local_refuses_to_boot() {
    let entra = TestIssuer::start().await;
    let mut cmd = std::process::Command::new(triton_tests::locate_triton_binary());
    for (k, _) in std::env::vars() {
        if k.starts_with("TRITON_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .env("TRITON_ENV", "nonprod")
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", triton_tests::free_tcp_port().to_string())
        .env("TRITON_A2A_PORT", triton_tests::free_tcp_port().to_string())
        .env(
            "TRITON_REST_PORT",
            triton_tests::free_tcp_port().to_string(),
        )
        .env(
            "TRITON_METRICS_PORT",
            triton_tests::free_tcp_port().to_string(),
        )
        .env("TRITON_ENTRA_AUDIENCE", ENTRA_AUD)
        .env("TRITON_ENTRA_ALLOWED_TENANTS", TID_ACME)
        .env("TRITON_ENTRA_AUTHORITY", entra.issuer_url())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run triton");
    // A binary that ignores the override boots and serves; give it a few
    // seconds, then stop it and fail rather than hang.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("try_wait").is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().expect("collect output");
    assert!(!out.status.success(), "boot must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("TRITON_ENTRA_AUTHORITY") || stdout.contains("TRITON_ENTRA_AUTHORITY"),
        "the refusal names the variable; stderr: {stderr} stdout: {stdout}"
    );
}

/// The Teams bot identity (`identity.kind: azure`): Bot Framework vouches
/// for the Entra user (`from.aadObjectId`, `channelData.tenant.id`), so the
/// principal names provider `entra` and the tenant id, and forwarding puts
/// both on the upstream token. The activity carries no email: Triton asks
/// the Bot Connector for the conversation member once (get-member by
/// `aadObjectId`), caches it per (tid, aadObjectId), and forwards its email.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_teams_bot_identity_forwards_entra_and_the_tenant_id() {
    use triton_tests::chat_courier_fixture::FakeBotFramework;
    let fake = FakeBotFramework::start().await;
    fake.add_member(
        "aad-obj-alice",
        json!({ "id": "29:zzz", "name": "Alice", "aadObjectId": "aad-obj-alice",
            "email": "Alice@Acme.Example", "userPrincipalName": "alice@acme.example",
            "tenantId": "tenant-guid-acme" }),
    );
    let agent = FakeAgent::start_returning(json!({ "surface": { "text": "ok" } })).await;
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-azure-upstream.yaml");
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            manifest.display().to_string(),
        ),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("answer={}", agent.host_port()),
        ),
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), SIGNER_JWKS.to_string()),
        (
            "TRITON_JWT_KID".to_string(),
            "triton-test-signer".to_string(),
        ),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAM_FORWARD_PRINCIPAL".to_string(),
            "true".to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com", "aud": "triton-msteams-test-appid",
        "exp": now() + 600, "iat": now() - 5, "serviceurl": format!("{}/", fake.url()),
    }));
    let webhook = triton.chat_webhook_addr.expect("webhook");
    for _ in 0..2 {
        let resp = reqwest::Client::new()
            .post(format!("http://{webhook}/msteams/webhook"))
            .header("Authorization", format!("Bearer {jwt}"))
            .json(&json!({
                "type": "message", "id": "msg-1",
                "timestamp": "2026-10-31T10:00:00.0000000Z",
                "serviceUrl": "https://placeholder.example/", "channelId": "msteams",
                "from": { "id": "29:zzz", "name": "Alice", "aadObjectId": "aad-obj-alice" },
                "conversation": { "id": "a:conv-azure", "conversationType": "personal" },
                "recipient": { "id": "28:bot-1", "name": "MyBot" },
                "channelData": { "tenant": { "id": "tenant-guid-acme" } },
                "text": "hello", "textFormat": "plain"
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while agent.bearers_seen().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the upstream was not called"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let claims = last_upstream_claims(&agent);
    assert_eq!(claims["sub"], "aad-obj-alice");
    assert_eq!(claims["tenant"], "tenant-guid-acme");
    assert_eq!(claims["idp"], "entra");
    assert_eq!(claims["idp_tenant"], "tenant-guid-acme");
    assert_eq!(
        claims["email"], "alice@acme.example",
        "from get-member, lower-cased"
    );
    assert_eq!(
        fake.member_lookups(),
        vec!["aad-obj-alice".to_string()],
        "looked up once, then cached"
    );
}

/// F9: under `TRITON_GOOGLE_ACCESS_ALLOWED_HD` only a Workspace member (a
/// real `hd`) is admitted — a consumer account on the customer's email
/// domain is not — and a bare hd maps to itself as its data tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn google_requires_a_real_hd_and_a_bare_hd_is_its_own_tenant() {
    let r = rig().await;
    assert_eq!(
        call(&r.triton, GOOGLE_TOKEN_NO_HD).await,
        reqwest::StatusCode::UNAUTHORIZED,
        "no hd, only a matching email domain"
    );

    let agent = FakeAgent::start_echoing().await;
    let google = FakeTokeninfo::start().await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_GOOGLE_ACCESS_AUDIENCES".to_string(),
            GOOGLE_CLIENT.to_string(),
        ),
        (
            "TRITON_GOOGLE_ACCESS_ALLOWED_HD".to_string(),
            "acme.example".to_string(),
        ),
        ("TRITON_GOOGLE_TOKENINFO_URL".to_string(), google.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
        (
            "TRITON_JWT_SIGNING_KEY".to_string(),
            SIGNING_KEY_PEM.to_string(),
        ),
        ("TRITON_JWT_JWKS".to_string(), SIGNER_JWKS.to_string()),
        (
            "TRITON_JWT_KID".to_string(),
            "triton-test-signer".to_string(),
        ),
        (
            "TRITON_SELF_ISSUER".to_string(),
            "https://triton.test".to_string(),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    assert_eq!(call(&triton, GOOGLE_TOKEN).await, reqwest::StatusCode::OK);
    let claims = last_upstream_claims(&agent);
    assert_eq!(
        claims["tenant"], "acme.example",
        "a bare hd is its own tenant"
    );
}
