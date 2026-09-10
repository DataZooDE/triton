//! #282 / crew F2+F3 — who may read a task through A2A `tasks/get`.
//!
//! `tasks/get` gates on [`may_read_trace`], which asks whether the caller
//! has at least one VISIBLE audit entry for the trace. Its predicate is
//!
//! ```text
//! operator || e.subject == sub || (tenant_scopable && e.tenant == tenant)
//! ```
//!
//! and a crew review found two defects in it that `/v1/audit`'s own tests
//! could not see, because they only exercise `/v1/audit`:
//!
//!   * **The middle disjunct is untenanted.** `AuditEntry.subject` is the
//!     bare `principal.sub` (`dispatcher.rs:1050`), so two principals in
//!     different tenants who happen to share a `sub` string read each
//!     other's tasks. Task ids ARE trace ids and travel to counterparties
//!     in every `message/send` reply, so the id is not a secret.
//!   * **The operator conjunction is duplicated here.** `may_read_trace`
//!     re-implements `scope && (local || named)` independently of
//!     `audit_visibility_in`. Deleting the `named` half left every one of
//!     the four `/v1/audit` tests green while `tasks/get` handed other
//!     tenants' traces to a bare `audit:read-all` holder.
//!
//! Both are pinned below, on the EMBEDDED surface — which is the shape
//! that runs in production and the shape where a control last shipped
//! dead (doc/realizations.md §9).
//!
//! Real HTTP, real RS256 verification against the in-repo issuer, real
//! dispatches through the real task store.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::principal::ToolPrincipal;
use triton_core::{Dispatcher, Tool, ToolRegistry};
use triton_embed::{EmbedOpts, router};
use triton_tests::TestIssuer;

const AUD: &str = "a2a-authz-audience";
const PUBLIC_URL: &str = "https://agent.example.test";

struct AssistantTool;

#[async_trait]
impl Tool for AssistantTool {
    fn name(&self) -> &'static str {
        "assistant"
    }
    async fn invoke(&self, args: Value, _p: &ToolPrincipal) -> Result<Value, TritonError> {
        let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
        Ok(json!({ "surface": { "text": format!("you said: {msg}") } }))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A token for `sub` in `tenant`. `scope` may be empty.
fn token(issuer: &TestIssuer, sub: &str, tenant: &str, scope: &str) -> String {
    issuer.sign_jwt(json!({
        "iss": issuer.issuer_url(),
        "aud": AUD,
        "sub": sub,
        "tenant": tenant,
        "scope": scope,
        "exp": now() + 600,
        "iat": now() - 5,
    }))
}

/// A spec-A2A host in a NON-`local` env, with the operator list passed
/// explicitly rather than through the process environment — parallel
/// tests must not race one global.
async fn host(issuer: &TestIssuer, operators: Vec<(String, String)>) -> String {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(AssistantTool));
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(reg),
        "nonprod".to_string(),
        DispatchControls::unenforced(),
    ));
    let opts = EmbedOpts::dev()
        .env("nonprod")
        .oidc(issuer.issuer_url(), AUD, None)
        .audit_operators(operators)
        .spec_a2a(
            "DataZoo Agent",
            "Answers questions.",
            PUBLIC_URL,
            "assistant",
        );
    let app = router(dispatcher, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn rpc(base: &str, tok: &str, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/a2a"))
        .bearer_auth(tok)
        .json(&body)
        .send()
        .await
        .expect("POST /a2a")
        .json()
        .await
        .unwrap_or(Value::Null)
}

/// Send a message and return the task id it created.
async fn create_task(base: &str, tok: &str, text: &str) -> String {
    let resp = rpc(
        base,
        tok,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": { "message": {
                "role": "user", "messageId": "m-1",
                "parts": [{ "kind": "text", "text": text }]
            } }
        }),
    )
    .await;
    // `message/send` answers with a Message whose `taskId` names the task
    // the reply belongs to — and that id is a TRACE id, which is exactly
    // why it is not a secret: it travels to the counterparty in every
    // reply.
    resp["result"]["taskId"]
        .as_str()
        .unwrap_or_else(|| panic!("no taskId in {resp}"))
        .to_string()
}

async fn get_task(base: &str, tok: &str, id: &str) -> Value {
    rpc(
        base,
        tok,
        json!({"jsonrpc":"2.0","id":2,"method":"tasks/get","params":{"id":id}}),
    )
    .await
}

/// The leak: same `sub`, different tenants.
///
/// `AuditEntry.subject` carries no tenant, so an untenanted subject match
/// lets `alice` in `globex` read `alice` in `acme`'s task. A colliding
/// `sub` is ordinary — chat adapters derive it from platform sender ids,
/// and two issuers can mint the same string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_colliding_subject_in_another_tenant_cannot_read_the_task() {
    let issuer = TestIssuer::start().await;
    let base = host(&issuer, vec![]).await;

    let acme = token(&issuer, "alice", "acme", "chat");
    let globex = token(&issuer, "alice", "globex", "chat");

    let id = create_task(&base, &acme, "acme business").await;
    let resp = get_task(&base, &globex, &id).await;

    assert!(
        resp["error"].is_object(),
        "`alice` in `globex` must not read `alice` in `acme`'s task — the \
         subject match has to be tenant-qualified; got {resp}"
    );
}

/// The owner still reads their own task. A tenant-qualified match must
/// not lock out the caller it is meant to admit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_still_reads_their_own_task() {
    let issuer = TestIssuer::start().await;
    let base = host(&issuer, vec![]).await;

    let acme = token(&issuer, "alice", "acme", "chat");
    let id = create_task(&base, &acme, "acme business").await;
    let resp = get_task(&base, &acme, &id).await;

    assert!(
        resp["result"].is_object(),
        "the task's own creator must still read it; got {resp}"
    );
}

/// A caller on the RESERVED tenant reads their own task too. `-` matches
/// nothing as a tenant, so this case rests entirely on the subject
/// disjunct — and it is nearly every live OIDC caller, since a token with
/// no `tenant` claim lands there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reserved_tenant_caller_still_reads_their_own_task() {
    let issuer = TestIssuer::start().await;
    let base = host(&issuer, vec![]).await;

    let unattributed = token(&issuer, "alice", "-", "chat");
    let id = create_task(&base, &unattributed, "no tenant claim").await;
    let resp = get_task(&base, &unattributed, &id).await;

    assert!(
        resp["result"].is_object(),
        "a caller on the reserved tenant must still read their OWN task; \
         got {resp}"
    );
}

/// The conjunction, on THIS surface: the scope alone does not open
/// another tenant's task when the operator list is empty.
///
/// The four `/v1/audit` tests cannot catch a regression here — deleting
/// the `named` half of `may_read_trace` leaves all of them green.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scope_alone_does_not_open_another_tenants_task() {
    let issuer = TestIssuer::start().await;
    let base = host(&issuer, vec![]).await;

    let acme = token(&issuer, "alice", "acme", "chat");
    let claimant = token(&issuer, "ops", "ops", "audit:read-all");

    let id = create_task(&base, &acme, "acme business").await;
    let resp = get_task(&base, &claimant, &id).await;

    assert!(
        resp["error"].is_object(),
        "an unnamed `audit:read-all` holder must not read another \
         tenant's task outside `local`; got {resp}"
    );
}

/// Both halves together do open it, so the gate stays passable for the
/// operator it exists for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_operator_holding_the_scope_reads_the_task() {
    let issuer = TestIssuer::start().await;
    let base = host(&issuer, vec![("ops".to_string(), "ops".to_string())]).await;

    let acme = token(&issuer, "alice", "acme", "chat");
    let operator = token(&issuer, "ops", "ops", "audit:read-all");

    let id = create_task(&base, &acme, "acme business").await;
    let resp = get_task(&base, &operator, &id).await;

    assert!(
        resp["result"].is_object(),
        "a named operator holding the scope must read the task; got {resp}"
    );
}
