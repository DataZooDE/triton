//! One MCP endpoint per customer, each discoverable as an RFC 9728 OAuth
//! protected resource whose authorization server is that customer's Entra
//! directory (author decision 2026-10-31: path per customer).
//!
//! Why: Claude and ChatGPT connectors find the authorization server only
//! through protected resource metadata — the `resource_metadata` pointer in
//! the 401 challenge (RFC 9728 §5.1), else the well-known URI with the
//! endpoint path inserted (RFC 9728 §3.1). Entra has no common issuer a
//! client can sign in against for "some customer", so each customer gets
//! its own path, `/mcp/<slug>`, whose metadata names
//! `https://login.microsoftonline.com/<tid>/v2.0`.
//!
//! The path is also a tenant binding: a token presented at `/mcp/<slug>`
//! must come from that slug's Entra tenant and maps to that slug's data
//! tenant. A valid token from another customer's directory is refused with
//! 401 `invalid_token`, as is any token from a non-Entra issuer.
//!
//! No mocks: the real binary, a `TestIssuer` that plays the Entra authority
//! for two tenant ids, a deployment OIDC `TestIssuer`, and a `FakeAgent`
//! upstream that records the minted upstream JWTs.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

use crate::inbound_issuers::{FakeTokeninfo, GOOGLE_CLIENT, GOOGLE_TOKEN};

const SIGNING_KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_key.pem"
));
const SIGNER_JWKS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/upstream_signer_jwks.json"
));

const ORIGIN: &str = "https://agents.example.test";
const OIDC_AUD: &str = "triton-trio";
const ENTRA_AUD: &str = "api://triton-mcp";
const SCOPE: &str = "api://triton-mcp/access_as_user";
const TID_ACME: &str = "11111111-2222-3333-4444-abcdefabcdef";
const TID_GLOBEX: &str = "99999999-8888-7777-6666-555555555555";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Rig {
    triton: TritonProcess,
    agent: FakeAgent,
    oidc: TestIssuer,
    entra: TestIssuer,
    _google: FakeTokeninfo,
}

fn base_env(
    agent: &FakeAgent,
    oidc: &TestIssuer,
    entra: &TestIssuer,
    google: &FakeTokeninfo,
) -> HashMap<String, String> {
    HashMap::from([
        (
            "TRITON_GOOGLE_ACCESS_AUDIENCES".to_string(),
            GOOGLE_CLIENT.to_string(),
        ),
        (
            "TRITON_GOOGLE_ACCESS_ALLOWED_HD".to_string(),
            "acme.example=acme".to_string(),
        ),
        ("TRITON_GOOGLE_TOKENINFO_URL".to_string(), google.url()),
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_OIDC_ISSUER".to_string(), oidc.issuer_url()),
        ("TRITON_OIDC_AUDIENCE".to_string(), OIDC_AUD.to_string()),
        ("TRITON_ENTRA_AUDIENCE".to_string(), ENTRA_AUD.to_string()),
        (
            "TRITON_ENTRA_ALLOWED_TENANTS".to_string(),
            format!("{TID_ACME}=acme,{TID_GLOBEX}=globex"),
        ),
        ("TRITON_ENTRA_AUTHORITY".to_string(), entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL".to_string(), entra.jwks_url()),
        (
            "TRITON_MCP_TENANTS".to_string(),
            format!("acme=acme:{TID_ACME},globex=globex:{TID_GLOBEX}"),
        ),
        ("TRITON_MCP_PUBLIC_ORIGIN".to_string(), ORIGIN.to_string()),
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
    ])
}

async fn rig_with(single_port: bool, overrides: &[(&str, &str)]) -> Rig {
    let agent = FakeAgent::start_echoing().await;
    let oidc = TestIssuer::start().await;
    let entra = TestIssuer::start().await;
    let google = FakeTokeninfo::start().await;
    let mut env = base_env(&agent, &oidc, &entra, &google);
    if single_port {
        env.insert("TRITON_SINGLE_PORT".to_string(), "true".to_string());
    }
    for (k, v) in overrides {
        env.insert(k.to_string(), v.to_string());
    }
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;
    Rig {
        triton,
        agent,
        oidc,
        entra,
        _google: google,
    }
}

async fn rig() -> Rig {
    rig_with(true, &[]).await
}

fn entra_claims(entra: &TestIssuer, tid: &str) -> Value {
    json!({
        "iss": format!("{}/{tid}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD,
        "sub": "entra-pairwise-sub",
        "oid": "00000000-0000-0000-0000-0000000000aa",
        "tid": tid,
        "preferred_username": "bob@acme.example",
        "exp": now() + 600,
        "iat": now(),
        "scp": "access_as_user",
    })
}

fn entra_token(entra: &TestIssuer, tid: &str) -> String {
    entra.sign_jwt(entra_claims(entra, tid))
}

fn tools_call() -> Value {
    json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "assistant", "arguments": { "message": "hi" } } })
}

async fn post(
    url: &str,
    token: Option<&str>,
    body: Value,
) -> (reqwest::StatusCode, Option<String>) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.expect("POST");
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().expect("ASCII").to_string());
    (resp.status(), challenge)
}

async fn get_json(url: &str) -> (reqwest::StatusCode, Option<String>, Value) {
    let resp = reqwest::get(url).await.expect("GET");
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    (status, ct, resp.json().await.unwrap_or(Value::Null))
}

/// `Bearer k1="v1", k2="v2"` → {k1: v1, k2: v2}. Enough of RFC 7235 for
/// the quoted parameters Triton emits.
fn challenge_params(header: &str) -> HashMap<String, String> {
    let rest = header
        .strip_prefix("Bearer ")
        .unwrap_or_else(|| panic!("not a Bearer challenge: {header}"));
    let mut out = HashMap::new();
    let mut s = rest.trim();
    while !s.is_empty() {
        let (key, after) = s.split_once('=').expect("key=value");
        let after = after.strip_prefix('"').expect("quoted value");
        let end = after.find('"').expect("closing quote");
        out.insert(key.trim().to_string(), after[..end].to_string());
        s = after[end + 1..].trim_start_matches(',').trim();
    }
    out
}

fn upstream_claims(agent: &FakeAgent) -> Value {
    let jwt = agent.bearers_seen().pop().expect("the upstream was called");
    let payload = jwt.split('.').nth(1).expect("payload");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

fn metadata_url(slug: &str) -> String {
    format!("{ORIGIN}/.well-known/oauth-protected-resource/mcp/{slug}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_customer_path_has_protected_resource_metadata_naming_its_entra_directory() {
    let r = rig().await;
    for (slug, tid) in [("acme", TID_ACME), ("globex", TID_GLOBEX)] {
        let expected = json!({
            "resource": format!("{ORIGIN}/mcp/{slug}"),
            "authorization_servers": [format!("{}/{tid}/v2.0", r.entra.issuer_url())],
            "scopes_supported": [SCOPE],
            "bearer_methods_supported": ["header"],
        });
        // RFC 9728 §3.1: the well-known segment inserted before the path.
        let (status, ct, doc) = get_json(
            &r.triton
                .rest_url(&format!("/.well-known/oauth-protected-resource/mcp/{slug}")),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{slug}");
        assert!(
            ct.as_deref()
                .is_some_and(|c| c.starts_with("application/json")),
            "content-type {ct:?}"
        );
        assert_eq!(doc, expected, "{slug}");
        // The suffix form older clients construct from the endpoint URL.
        let (status, _, doc) = get_json(
            &r.triton
                .rest_url(&format!("/mcp/{slug}/.well-known/oauth-protected-resource")),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{slug} suffix form");
        assert_eq!(doc, expected, "{slug} suffix form");
    }
    let (status, _, _) = get_json(
        &r.triton
            .rest_url("/.well-known/oauth-protected-resource/mcp/initech"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "unknown slug");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_401_on_a_customer_path_points_at_its_metadata() {
    let r = rig().await;
    let url = r.triton.rest_url("/mcp/acme");
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": { "protocolVersion": "2025-06-18" } });

    let (status, challenge) = post(&url, None, init.clone()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let p = challenge_params(&challenge.expect("WWW-Authenticate"));
    assert_eq!(p.get("realm").map(String::as_str), Some("triton"));
    assert_eq!(p.get("resource_metadata"), Some(&metadata_url("acme")));
    assert_eq!(p.get("scope").map(String::as_str), Some(SCOPE));
    assert!(!p.contains_key("error"), "no credentials → bare challenge");

    let (status, challenge) = post(&url, Some("not-a-jwt"), init).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let p = challenge_params(&challenge.expect("WWW-Authenticate"));
    assert_eq!(p.get("error").map(String::as_str), Some("invalid_token"));
    assert_eq!(p.get("resource_metadata"), Some(&metadata_url("acme")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_from_the_customers_directory_reaches_the_upstream_as_its_data_tenant() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_GLOBEX);
    let (status, _) = post(
        &r.triton.rest_url("/mcp/globex"),
        Some(&token),
        tools_call(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let claims = upstream_claims(&r.agent);
    assert_eq!(claims["tenant"], "globex");
    assert_eq!(claims["idp"], "entra");
    assert_eq!(claims["idp_tenant"], TID_GLOBEX);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_from_another_customers_directory_is_refused() {
    let r = rig().await;
    // Valid, allow-listed, from globex's directory — presented at acme.
    let token = entra_token(&r.entra, TID_GLOBEX);
    let (status, challenge) =
        post(&r.triton.rest_url("/mcp/acme"), Some(&token), tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let p = challenge_params(&challenge.expect("WWW-Authenticate"));
    assert_eq!(p.get("error").map(String::as_str), Some("invalid_token"));
    assert_eq!(p.get("resource_metadata"), Some(&metadata_url("acme")));
    assert!(r.agent.bearers_seen().is_empty(), "no dispatch happened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_customer_path_admits_only_entra_while_plain_mcp_keeps_every_issuer() {
    let r = rig().await;
    let own = r.oidc.sign_jwt(json!({
        "iss": r.oidc.issuer_url(), "aud": OIDC_AUD, "sub": "carol",
        "tenant": "acme", "exp": now() + 600,
    }));
    let (status, _) = post(&r.triton.rest_url("/mcp/acme"), Some(&own), tools_call()).await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "a deployment-issuer token claiming the tenant is not the customer's directory"
    );
    assert!(r.agent.bearers_seen().is_empty());

    // `/mcp` is the single-issuer mode as before: any configured issuer,
    // and its challenge names no metadata.
    let (status, _) = post(&r.triton.rest_url("/mcp"), Some(&own), tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (status, challenge) = post(&r.triton.rest_url("/mcp"), None, tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(challenge.as_deref(), Some(r#"Bearer realm="triton""#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_customer_paths_are_served_on_the_mcp_listener_in_three_port_mode() {
    let r = rig_with(false, &[]).await;
    let (status, _, doc) = get_json(
        &r.triton
            .mcp_url("/.well-known/oauth-protected-resource/mcp/acme"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(doc["resource"], format!("{ORIGIN}/mcp/acme"));
    let token = entra_token(&r.entra, TID_ACME);
    let (status, _) = post(&r.triton.mcp_url("/mcp/acme"), Some(&token), tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(upstream_claims(&r.agent)["tenant"], "acme");
}

/// The delegated scope is required: an app-only (client-credentials) token
/// from the customer's own tenant — right `iss`, `tid` and `aud`, `roles`
/// and no `scp` — has no user behind it, and a user token for another
/// permission is not this one. RFC 6750 §3.1 / MCP: 403
/// `insufficient_scope`, naming the scope and the metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_without_the_delegated_scope_is_refused_with_insufficient_scope() {
    let r = rig().await;
    let mut app_only = entra_claims(&r.entra, TID_ACME);
    let c = app_only.as_object_mut().unwrap();
    c.remove("scp");
    c.remove("preferred_username");
    c.insert("roles".into(), json!(["Tools.Execute.All"]));
    c.insert("idtyp".into(), json!("app"));
    let mut other_scope = entra_claims(&r.entra, TID_ACME);
    other_scope["scp"] = json!("User.Read");
    for (label, claims) in [("app-only", app_only), ("other scope", other_scope)] {
        let token = r.entra.sign_jwt(claims);
        let (status, challenge) =
            post(&r.triton.rest_url("/mcp/acme"), Some(&token), tools_call()).await;
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{label}");
        let p = challenge_params(&challenge.expect("WWW-Authenticate"));
        assert_eq!(
            p.get("error").map(String::as_str),
            Some("insufficient_scope"),
            "{label}"
        );
        assert_eq!(p.get("scope").map(String::as_str), Some(SCOPE), "{label}");
        assert_eq!(p.get("resource_metadata"), Some(&metadata_url("acme")));
    }
    assert!(r.agent.bearers_seen().is_empty(), "no dispatch happened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_google_token_and_a_non_canonical_entra_issuer_are_refused_on_a_customer_path() {
    let r = rig().await;
    let (status, challenge) = post(
        &r.triton.rest_url("/mcp/acme"),
        Some(GOOGLE_TOKEN),
        tools_call(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "Google, hd=acme");
    let p = challenge_params(&challenge.expect("WWW-Authenticate"));
    assert_eq!(p.get("error").map(String::as_str), Some("invalid_token"));

    // Entra issues lower-case tenant ids; an upper-case one in `iss` is not
    // the advertised authorization server and fails closed.
    let mut claims = entra_claims(&r.entra, TID_ACME);
    claims["iss"] = json!(format!(
        "{}/{}/v2.0",
        r.entra.issuer_url(),
        TID_ACME.to_ascii_uppercase()
    ));
    let token = r.entra.sign_jwt(claims);
    let (status, _) = post(&r.triton.rest_url("/mcp/acme"), Some(&token), tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "upper-case iss");
    assert!(r.agent.bearers_seen().is_empty(), "no dispatch happened");

    // The same Google token is still good on plain `/mcp`.
    let (status, _) = post(&r.triton.rest_url("/mcp"), Some(GOOGLE_TOKEN), tools_call()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
}

/// The advertised `resource` is the canonical origin (lower-case scheme and
/// host, no default port): Claude compares it with the canonical URL and
/// Entra matches the Application ID URI exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_public_origin_is_normalised() {
    let r = rig_with(
        true,
        &[(
            "TRITON_MCP_PUBLIC_ORIGIN",
            "HTTPS://Agents.Example.TEST:443/",
        )],
    )
    .await;
    let (_, _, doc) = get_json(
        &r.triton
            .rest_url("/.well-known/oauth-protected-resource/mcp/acme"),
    )
    .await;
    assert_eq!(doc["resource"], format!("{ORIGIN}/mcp/acme"));
    let (_, challenge) = post(&r.triton.rest_url("/mcp/acme"), None, tools_call()).await;
    let p = challenge_params(&challenge.expect("WWW-Authenticate"));
    assert_eq!(p.get("resource_metadata"), Some(&metadata_url("acme")));
}

/// Boot-time consistency: each row is a configuration the binary must
/// refuse, and the refusal must name the variable(s) at fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inconsistent_customer_route_refuses_to_boot() {
    let agent = FakeAgent::start_echoing().await;
    let oidc = TestIssuer::start().await;
    let entra = TestIssuer::start().await;
    let google = FakeTokeninfo::start().await;
    let ok = format!("acme=acme:{TID_ACME}");
    let entra_issuer = format!("{}/{TID_ACME}/v2.0", entra.issuer_url());
    type Row<'a> = (&'a str, Vec<(&'a str, Option<String>)>, Vec<&'a str>);
    let rows: Vec<Row> = vec![
        (
            "tid not allow-listed",
            vec![(
                "TRITON_MCP_TENANTS",
                Some("acme=acme:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            )],
            vec!["TRITON_MCP_TENANTS", "TRITON_ENTRA_ALLOWED_TENANTS"],
        ),
        (
            "tid maps to another data tenant",
            vec![(
                "TRITON_MCP_TENANTS",
                Some(format!("acme=globex:{TID_ACME}")),
            )],
            vec!["TRITON_MCP_TENANTS"],
        ),
        (
            "duplicate slug",
            vec![("TRITON_MCP_TENANTS", Some(format!("{ok},{ok}")))],
            vec!["TRITON_MCP_TENANTS", "twice"],
        ),
        (
            "upper-case slug",
            vec![("TRITON_MCP_TENANTS", Some(format!("Acme=acme:{TID_ACME}")))],
            vec!["TRITON_MCP_TENANTS", "slug"],
        ),
        (
            "slug with a slash",
            vec![("TRITON_MCP_TENANTS", Some(format!("a/b=acme:{TID_ACME}")))],
            vec!["TRITON_MCP_TENANTS", "slug"],
        ),
        (
            "no public origin",
            vec![("TRITON_MCP_PUBLIC_ORIGIN", None)],
            vec!["TRITON_MCP_PUBLIC_ORIGIN"],
        ),
        (
            "origin with a path",
            vec![("TRITON_MCP_PUBLIC_ORIGIN", Some(format!("{ORIGIN}/base")))],
            vec!["TRITON_MCP_PUBLIC_ORIGIN"],
        ),
        (
            "origin with userinfo",
            vec![(
                "TRITON_MCP_PUBLIC_ORIGIN",
                Some("https://u:p@agents.example.test".into()),
            )],
            vec!["TRITON_MCP_PUBLIC_ORIGIN", "userinfo"],
        ),
        (
            "no Entra verifier",
            vec![("TRITON_ENTRA_AUDIENCE", None)],
            vec!["TRITON_MCP_TENANTS", "TRITON_ENTRA_AUDIENCE"],
        ),
        (
            "scope outside the RFC 6749 grammar",
            vec![("TRITON_ENTRA_SCOPES", Some("api://x/a\"b".into()))],
            vec!["TRITON_ENTRA_SCOPES"],
        ),
        (
            "deployment issuer under the Entra authority",
            vec![("TRITON_OIDC_ISSUER", Some(entra_issuer.clone()))],
            vec!["TRITON_MCP_TENANTS", "TRITON_OIDC_ISSUER"],
        ),
    ];
    for (label, changes, expected) in rows {
        let mut env = base_env(&agent, &oidc, &entra, &google);
        for (k, v) in changes {
            match v {
                Some(v) => env.insert(k.to_string(), v),
                None => env.remove(k),
            };
        }
        let mut cmd = tokio::process::Command::new(triton_tests::locate_triton_binary());
        for (k, _) in std::env::vars() {
            if k.starts_with("TRITON_") {
                cmd.env_remove(k);
            }
        }
        let port = || triton_tests::free_tcp_port().to_string();
        let child = cmd
            .envs(&env)
            .env("TRITON_HOST", "127.0.0.1")
            .env("TRITON_MCP_PORT", port())
            .env("TRITON_A2A_PORT", port())
            .env("TRITON_REST_PORT", port())
            .env("TRITON_METRICS_PORT", port())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("run triton");
        // A binary that boots despite the row serves forever: give it ten
        // seconds, then fail rather than hang (kill_on_drop stops it).
        let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap_or_else(|_| panic!("{label}: booted instead of refusing"))
            .expect("output");
        assert!(!out.status.success(), "{label}: boot must fail");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        for needle in expected {
            assert!(
                text.contains(needle),
                "{label}: the refusal names `{needle}`: {text}"
            );
        }
    }
}

/// The tool names `tools/list` returns at `url` for `token`.
async fn tool_names(url: &str, token: &str) -> Vec<String> {
    let resp = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
        .send()
        .await
        .expect("POST tools/list");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let text = resp.text().await.expect("body");
    // A JSON body, or one SSE `data:` event carrying it.
    let json = text
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .unwrap_or(&text);
    let body: Value = serde_json::from_str(json.trim()).expect("JSON-RPC response");
    let mut names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array: {body}"))
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    names.sort();
    names
}

/// By default the in-process demo tools (`echo`, ...) are listed next to
/// the upstream's on a customer path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_customer_path_lists_the_built_in_tools_by_default() {
    let r = rig().await;
    let token = entra_token(&r.entra, TID_ACME);
    let names = tool_names(&r.triton.rest_url("/mcp/acme"), &token).await;
    assert!(names.contains(&"assistant".to_string()), "{names:?}");
    assert!(names.contains(&"echo".to_string()), "{names:?}");
}

/// `TRITON_BUILTIN_TOOLS=off` registers no in-process tool: a public
/// gateway in front of one agent lists exactly that agent's tools, so an
/// MCP client (Claude, ChatGPT) does not offer `echo` or `demo_panel` to
/// the user. Calling a built-in name is then an unknown tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_tools_off_lists_only_the_upstream_tools() {
    let r = rig_with(true, &[("TRITON_BUILTIN_TOOLS", "off")]).await;
    let token = entra_token(&r.entra, TID_ACME);
    for path in ["/mcp/acme", "/mcp/globex"] {
        let token = if path.ends_with("globex") {
            entra_token(&r.entra, TID_GLOBEX)
        } else {
            token.clone()
        };
        let names = tool_names(&r.triton.rest_url(path), &token).await;
        assert_eq!(names, vec!["assistant".to_string()], "{path}");
    }

    let resp = reqwest::Client::new()
        .post(r.triton.rest_url("/mcp/acme"))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(&token)
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                       "params": { "name": "echo", "arguments": { "message": "hi" } } }))
        .send()
        .await
        .expect("POST tools/call");
    let text = resp.text().await.expect("body");
    assert!(
        !text.contains("\"echo\":\"hi\""),
        "a disabled built-in must not run: {text}"
    );
}

/// An unknown value refuses to boot rather than silently exposing the
/// built-ins on a gateway the operator meant to lock down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_builtin_tools_value_refuses_to_boot() {
    let mut cmd = tokio::process::Command::new(triton_tests::locate_triton_binary());
    for (k, _) in std::env::vars() {
        if k.starts_with("TRITON_") {
            cmd.env_remove(k);
        }
    }
    let port = || triton_tests::free_tcp_port().to_string();
    let child = cmd
        .env("TRITON_BUILTIN_TOOLS", "maybe")
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", port())
        .env("TRITON_A2A_PORT", port())
        .env("TRITON_REST_PORT", port())
        .env("TRITON_METRICS_PORT", port())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("run triton");
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("booted instead of refusing")
        .expect("output");
    assert!(!out.status.success(), "boot must fail");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(text.contains("TRITON_BUILTIN_TOOLS"), "{text}");
}
