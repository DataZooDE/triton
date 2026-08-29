//! #212 — the Teams adapter's outbound credential as an Entra
//! FEDERATED credential instead of a client_secret.
//!
//! This is the shape `dz-agent-template` runs in production: the pod
//! holds no static secret at all, only its projected Kubernetes
//! ServiceAccount token (`AZURE_FEDERATED_TOKEN_FILE`), which Entra
//! accepts as an RFC 7523 `client_assertion` because a federated
//! credential on the bot's app registration trusts the cluster's
//! OIDC issuer.
//!
//! No mocks per CLAUDE.md §1: real binary, real HTTP, real RS256
//! verification, and the `FakeBotFramework` token endpoint records
//! the actual form body so the test asserts WHICH grant was used —
//! "a token came back" would pass even if the client had silently
//! fallen back to a secret.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;

const AUDIENCE: &str = "triton-msteams-test-appid";
const BOT_ISSUER: &str = "https://api.botframework.com";
const TENANT_ID: &str = "28c0071d-815c-4ace-a3b5-9a28bde005fd";
/// Stand-in for a projected ServiceAccount token. Shape matters
/// (three dot-separated segments), content does not — the fixture
/// does not verify the assertion, Entra would.
const FAKE_PROJECTED_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.ZmFrZS1zYS10b2tlbg.c2ln";

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-federated-test.yaml")
        .display()
        .to_string()
}

/// A temp directory that removes itself on drop. `tempfile` is not a
/// dependency of this crate, and one test dir does not justify adding
/// one to every link of the aggregated `it` binary.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "triton-msteams-fed-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write the token where a projected volume would mount it. Returns
/// the directory so the caller keeps it alive for the process's
/// lifetime.
fn write_token_file() -> (TempDir, String) {
    let dir = TempDir::new("boot");
    let path = dir.file("token");
    // No trailing newline: this is what kubelet actually projects.
    std::fs::write(&path, FAKE_PROJECTED_TOKEN).expect("write token");
    let p = path.display().to_string();
    (dir, p)
}

fn env_with(fake: &FakeBotFramework, token_file: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_FEDERATED_TOKEN_FILE".to_string(),
            token_file.to_string(),
        ),
        (
            "TRITON_MSTEAMS_TENANT_ID".to_string(),
            TENANT_ID.to_string(),
        ),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn message_activity(text: &str) -> Value {
    json!({
        "type": "message",
        "id": "msg-1",
        "timestamp": "2026-05-25T10:00:00.0000000Z",
        "serviceUrl": "https://placeholder.example/",
        "channelId": "msteams",
        "from": { "id": "29:1abc", "name": "Alice" },
        "conversation": { "id": "a:conv-1", "conversationType": "personal" },
        "recipient": { "id": "28:bot-1", "name": "MyBot" },
        "text": text,
        "textFormat": "plain"
    })
}

fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for condition");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federated_credential_mints_a_token_and_couriers_the_reply() {
    let fake = FakeBotFramework::start().await;
    let (_dir, token_file) = write_token_file();
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake, &token_file)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    let jwt = fake.sign_jwt(json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
    }));

    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("hello over federation"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success(), "{}", resp.status());

    // The reply reaches Bot Framework — proving the whole outbound
    // path worked, token included.
    let captured = wait_for(Duration::from_secs(5), || {
        let v = fake.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(captured.len(), 1);
    assert!(captured[0].bearer.starts_with("Bearer "));

    // The grant was the federated one. This is the assertion that
    // actually distinguishes #212 from the status quo.
    let reqs = fake.token_requests();
    assert!(!reqs.is_empty(), "token endpoint was never called");
    let body = &reqs[0];
    assert!(
        body.contains(
            "client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer"
        ),
        "federated grant must send the RFC 7523 assertion type; got: {body}"
    );
    assert!(
        body.contains("client_assertion="),
        "federated grant must send the assertion; got: {body}"
    );
    assert!(
        !body.contains("client_secret"),
        "a federated client must never send a client_secret; got: {body}"
    );
    assert!(
        body.contains(FAKE_PROJECTED_TOKEN),
        "the assertion must be the projected token file's contents; got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assertion_is_reread_per_refresh_not_cached_at_boot() {
    // kubelet rotates the projected token. If the client cached the
    // file contents at construction, everything would work for an
    // hour and then fail in production — so prove the read happens
    // at refresh time by rewriting the file before the first mint.
    let fake = FakeBotFramework::start().await;
    let dir = TempDir::new("rotate");
    let path = dir.file("token");
    std::fs::write(&path, "eyJhbGciOiJSUzI1NiJ9.b2xk.c2ln").expect("write");
    let token_file = path.display().to_string();

    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(&fake, &token_file)).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");

    // Rotate AFTER boot, BEFORE the first outbound token mint.
    const ROTATED: &str = "eyJhbGciOiJSUzI1NiJ9.cm90YXRlZA.c2ln";
    std::fs::write(&path, ROTATED).expect("rotate");

    let jwt = fake.sign_jwt(json!({
        "iss": BOT_ISSUER,
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
    }));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&message_activity("rotate me"))
        .send()
        .await
        .expect("POST");
    assert!(resp.status().is_success());

    let reqs = wait_for(Duration::from_secs(5), || {
        let r = fake.token_requests();
        (!r.is_empty()).then_some(r)
    });
    assert!(
        reqs[0].contains(ROTATED),
        "the rotated token must be used, not the one present at boot; got: {}",
        reqs[0]
    );
}
