//! In-process upstream parity: a host that embeds Triton and dispatches to
//! its agents in-process (an `UpstreamDispatch` implemented in the host)
//! presents the same faces as the `triton` binary dispatching to the same
//! agents over HTTP through `TRITON_STATIC_UPSTREAMS`.
//!
//! Two hosts, one configuration:
//!
//! * reference — the real `triton` binary, single port, its upstreams the
//!   HTTP fakes (`FakeAsyncAgent` for a long-running task whose stream
//!   breaks, `FakeAgent` answering a card, `FakeAgent` failing with 500);
//! * embedded — `triton_embed::router_parts` with the same ingress (parsed
//!   from the same `TRITON_*` names as flags), its upstream [`InProcess`],
//!   which answers as those fakes do through the `UpstreamDispatch`
//!   contract (doc/upstream-agent-contract.md, "In-process upstream").
//!
//! Compared: MCP `tools/list` (with the answer-card widget `_meta`) and
//! `resources/list`, a widget-carrying `tools/call` result, the A2A cards in
//! both versions, the RFC 9728 metadata and the 401/403 challenges of a
//! customer path, the error mapping of an agent fault and of an unknown
//! tool, and a task whose upstream stream breaks and is resumed — not
//! refiled — by the upstream task id. The agent also sees the same
//! `_triton.surface` over both transports.
//!
//! No mocks of the interface under test: the binary, the embedded router
//! and the fakes all run over real sockets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use triton_core::dispatcher::DispatchControls;
use triton_core::error::TritonError;
use triton_core::stream::StreamEvent;
use triton_core::{DispatchContext, Dispatcher, Principal, ToolRegistry, UpstreamDispatch};
use triton_embed::ingress::IngressConfig;
use triton_embed::{EmbedOpts, router_parts};
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const ORIGIN: &str = "https://agents.example.test";
const PUBLIC: &str = "https://agents.example.test/a2a";
const ENTRA_AUD: &str = "api://triton-mcp";
const TID_ACME: &str = "11111111-2222-3333-4444-abcdefabcdef";
const RECEIPT: &str = "Your request is filed and still running.";

fn card_answer() -> Value {
    json!({ "surface": { "components": [
        { "kind": "header", "title": "Top customer" },
        { "kind": "text", "value": "Initech leads at $2,500.75." },
        { "kind": "key_value", "pairs": [ { "label": "Customer", "value": "Initech" },
                                          { "label": "Revenue", "value": "$2,500.75" } ] },
        { "kind": "chips", "chips": [ { "label": "What does Initech buy?",
                                        "message": "What does Initech buy?" } ] }
    ] } })
}

fn task_result() -> Value {
    AsyncPlan::default().result
}

/// What the embedded host's agent loop received.
#[derive(Default)]
struct Seen {
    calls: Vec<(String, Value)>,
    resumes: Vec<String>,
}

/// The host's own agents, in-process: `answer` returns a card, `assistant`
/// goes asynchronous and its first stream breaks after one progress event,
/// `broken` is an agent fault. Each mirrors the HTTP fake of the same name.
struct InProcess {
    seen: Arc<Mutex<Seen>>,
}

fn progress(k: u64) -> StreamEvent {
    StreamEvent::Progress(json!({ "text": format!("Step {k} of 2"), "step": format!("{k}/2") }))
}

#[async_trait]
impl UpstreamDispatch for InProcess {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _p: &Principal,
        ctx: &DispatchContext,
    ) -> Result<Value, TritonError> {
        // The body an HTTP agent gets: `_triton.surface` included.
        let args = triton_core::surface_hint(args, ctx);
        self.seen
            .lock()
            .unwrap()
            .calls
            .push((tool.to_string(), args));
        match tool {
            "answer" => Ok(card_answer()),
            // A non-2xx answer is an agent fault: `Tool`, as StaticUpstream
            // maps it.
            "broken" => Err(TritonError::Tool(
                "upstream broken returned 500 Internal Server Error".into(),
            )),
            // A tool nobody serves is a caller fault: `Validation`.
            other => Err(TritonError::Validation(format!("unknown tool: {other}"))),
        }
    }

    async fn invoke_streaming(
        &self,
        tool: &str,
        args: Value,
        p: &Principal,
        ctx: &DispatchContext,
    ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
        if tool != "assistant" {
            let v = self.invoke(tool, args, p, ctx).await?;
            return Ok(futures::stream::once(async move { StreamEvent::Done(v) }).boxed());
        }
        let args = triton_core::surface_hint(args, ctx);
        self.seen
            .lock()
            .unwrap()
            .calls
            .push((tool.to_string(), args));
        // §7: `task`, a step, then the stream ends without a terminal
        // event — a broken connection the runner must resume.
        Ok(futures::stream::iter(vec![
            StreamEvent::Task(json!({ "task_id": "up-1", "check_after_seconds": 5,
                                      "receipt": RECEIPT })),
            progress(1),
        ])
        .boxed())
    }

    async fn resume_streaming(
        &self,
        tool: &str,
        task_id: &str,
        _args: Value,
        _p: &Principal,
        _ctx: &DispatchContext,
    ) -> Result<BoxStream<'static, StreamEvent>, TritonError> {
        if tool != "assistant" || task_id != "up-1" {
            return Err(TritonError::Tool(format!(
                "upstream {tool} returned 404 Not Found"
            )));
        }
        self.seen.lock().unwrap().resumes.push(task_id.to_string());
        let done = task_result();
        Ok(futures::stream::iter(vec![progress(2)])
            .chain(futures::stream::once(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                StreamEvent::Done(done)
            }))
            .boxed())
    }

    async fn list_agents(&self) -> Vec<String> {
        vec!["answer".into(), "assistant".into(), "broken".into()]
    }
}

/// The shared configuration, as the binary's environment.
fn ingress_env(entra: &TestIssuer) -> Vec<(&'static str, String)> {
    let skills = json!([{ "id": "assistant", "name": "Assistant",
                          "description": "Answers questions.", "tags": ["qa"] }]);
    vec![
        ("TRITON_ENTRA_AUDIENCE", ENTRA_AUD.into()),
        ("TRITON_ENTRA_ALLOWED_TENANTS", format!("{TID_ACME}=acme")),
        ("TRITON_ENTRA_AUTHORITY", entra.issuer_url()),
        ("TRITON_ENTRA_JWKS_URL", entra.jwks_url()),
        ("TRITON_MCP_TENANTS", format!("acme=acme:{TID_ACME}")),
        ("TRITON_MCP_PUBLIC_ORIGIN", ORIGIN.into()),
        ("TRITON_A2A_PUBLIC_URL", PUBLIC.into()),
        ("TRITON_A2A_DEFAULT_TOOL", "assistant".into()),
        ("TRITON_A2A_AGENT_NAME", "Parity agent".into()),
        (
            "TRITON_A2A_AGENT_DESCRIPTION",
            "One agent, two hosts.".into(),
        ),
        ("TRITON_A2A_AGENT_VERSION", "1.2.3".into()),
        ("TRITON_A2A_SKILLS", skills.to_string()),
        ("TRITON_MCP_ASYNC_WAIT_SECS", "1".into()),
    ]
}

/// `TRITON_FOO_BAR` → `--foo-bar`: the ingress flags carry the variable
/// names (clap `long` + `env`).
fn as_flags(env: &[(&'static str, String)]) -> Vec<String> {
    let mut argv = vec!["host".to_string()];
    for (k, v) in env {
        let flag = k
            .strip_prefix("TRITON_")
            .expect("a TRITON_ variable")
            .to_ascii_lowercase()
            .replace('_', "-");
        argv.push(format!("--{flag}"));
        argv.push(v.clone());
    }
    argv
}

struct Hosts {
    reference: String,
    embedded: String,
    async_agent: FakeAsyncAgent,
    seen: Arc<Mutex<Seen>>,
    entra: TestIssuer,
    _triton: TritonProcess,
    _fakes: (FakeAgent, FakeAgent),
}

async fn hosts() -> Hosts {
    let entra = TestIssuer::start().await;
    let async_agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(900),
        tick: Duration::from_millis(300),
        drop_after: Some(1),
        ..AsyncPlan::default()
    })
    .await;
    let answer = FakeAgent::start_returning(card_answer()).await;
    let broken = FakeAgent::start_always_failing().await;
    let shared = ingress_env(&entra);

    let mut env: HashMap<String, String> = shared
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    env.extend([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        ("TRITON_BUILTIN_TOOLS".to_string(), "off".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "answer={},assistant={},broken={}",
                answer.host_port(),
                async_agent.host_port(),
                broken.host_port()
            ),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(10), env).await;

    let seen = Arc::new(Mutex::new(Seen::default()));
    let dispatcher = Arc::new(
        Dispatcher::new(
            Arc::new(ToolRegistry::new()),
            "local".to_string(),
            DispatchControls::unenforced(),
        )
        .with_surfaces(triton_embed::surfaces::registry(None).expect("registry"))
        .with_upstream(Arc::new(InProcess { seen: seen.clone() })),
    );
    let cfg = IngressConfig::try_parse_from(as_flags(&shared)).expect("the same variables parse");
    let parts = router_parts(dispatcher, &EmbedOpts::dev().env("local").ingress(cfg))
        .expect("the embedded ingress is accepted");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = parts.router;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Hosts {
        reference: triton.rest_url(""),
        embedded: format!("http://{addr}"),
        async_agent,
        seen,
        entra,
        _triton: triton,
        _fakes: (answer, broken),
    }
}

async fn get(base: &str, path: &str) -> (u16, Value) {
    let resp = reqwest::get(format!("{base}{path}")).await.expect("GET");
    (
        resp.status().as_u16(),
        resp.json().await.unwrap_or(Value::Null),
    )
}

/// POST JSON-RPC; the status, the `WWW-Authenticate` header and the body
/// (the last SSE `data:` event when the answer is a stream).
async fn rpc(
    url: &str,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (u16, Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        // A2A 1.0 method names; MCP ignores the header.
        .header("A2A-Version", "1.0")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.expect("POST");
    let status = resp.status().as_u16();
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string());
    let text = resp.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or_else(|_| {
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .next_back()
            .unwrap_or(Value::Null)
    });
    (status, challenge, body)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn entra_token(entra: &TestIssuer, scp: &str) -> String {
    entra.sign_jwt(json!({
        "iss": format!("{}/{TID_ACME}/v2.0", entra.issuer_url()),
        "aud": ENTRA_AUD, "sub": "entra-pairwise-sub",
        "oid": "00000000-0000-0000-0000-0000000000aa", "tid": TID_ACME,
        "preferred_username": "bob@acme.example",
        "exp": now() + 600, "iat": now(), "scp": scp,
    }))
}

/// Replace the values that differ per run (trace and task ids) so two
/// answers compare by shape and content.
fn scrub(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let v = match k.as_str() {
                        "id" | "taskId" | "task_id" | "contextId" | "trace_id" | "traceId"
                        | "messageId" | "artifactId" | "timestamp" | "receiptId"
                            if v.is_string() =>
                        {
                            json!("<id>")
                        }
                        "latency_ms" | "ttfb_ms" => json!("<ms>"),
                        _ => scrub(v),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(scrub).collect()),
        Value::String(s) => {
            // Ids embedded in prose (a task id in a "filed" text).
            let mut out = s.clone();
            for marker in ["task ", "id "] {
                if let Some(i) = out.find(marker) {
                    let start = i + marker.len();
                    let end = out[start..]
                        .find(|c: char| c.is_whitespace() || c == ')' || c == ',' || c == '.')
                        .map_or(out.len(), |e| start + e);
                    if out[start..end].len() >= 16 {
                        out.replace_range(start..end, "<id>");
                    }
                }
            }
            Value::String(out)
        }
        other => other.clone(),
    }
}

fn by_name(tools: &Value) -> Vec<Value> {
    let mut v = tools.as_array().cloned().unwrap_or_default();
    v.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_process_upstream_behaves_like_a_static_upstream_on_every_face() {
    let h = hosts().await;
    let hosts = [("reference", &h.reference), ("embedded", &h.embedded)];
    // With the Entra verifier configured, OIDC is the only identity on
    // every face (the dev token is closed), so a delegated Entra token
    // drives the trio too.
    let bearer = entra_token(&h.entra, "access_as_user");
    let tok = Some(bearer.as_str());

    // 1. A long-running task whose first upstream stream breaks: resumed
    //    by the upstream task id, never filed twice, completed with the
    //    agent's answer. Done first, before any listing probes the agents.
    let mut finals = Vec::new();
    for (label, base) in hosts {
        let (status, _, sent) = rpc(
            &format!("{base}/a2a"),
            tok,
            "SendMessage",
            json!({ "message": { "role": "ROLE_USER", "messageId": format!("m-{label}"),
                                 "parts": [{ "text": "Propose owners" }] },
                    "configuration": { "returnImmediately": true } }),
        )
        .await;
        assert_eq!(status, 200, "{label}: {sent}");
        let id = sent["result"]["task"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: a task: {sent}"))
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(15);
        let done = loop {
            let (_, _, got) =
                rpc(&format!("{base}/a2a"), tok, "GetTask", json!({ "id": id })).await;
            let state = got["result"]["status"]["state"].as_str().unwrap_or("");
            if state == "TASK_STATE_COMPLETED" || state == "TASK_STATE_FAILED" {
                break got["result"].clone();
            }
            assert!(Instant::now() < deadline, "{label}: unfinished {got}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(
            done["status"]["state"], "TASK_STATE_COMPLETED",
            "{label}: {done}"
        );
        finals.push((done["status"]["state"].clone(), done["artifacts"].clone()));
    }
    assert_eq!(
        scrub(&json!(finals[0])),
        scrub(&json!(finals[1])),
        "the completed task is the same"
    );
    assert_eq!(h.async_agent.filed_count(), 1, "HTTP: filed once");
    // The HTTP fake breaks every stream after one step, so it may be
    // resumed more than once; always by the upstream task id.
    let resumes = h.async_agent.resumes();
    assert!(
        !resumes.is_empty() && resumes.iter().all(|r| r == "up-1"),
        "HTTP: resumed by the upstream task id: {resumes:?}"
    );
    {
        let seen = h.seen.lock().unwrap();
        let filed = seen.calls.iter().filter(|(t, _)| t == "assistant").count();
        assert_eq!(filed, 1, "in-process: filed once");
        assert_eq!(seen.resumes, vec!["up-1".to_string()]);
        // The same `_triton.surface` over both transports.
        let http_body = &h.async_agent.requests()[0].body;
        let (_, inproc_args) = seen.calls.iter().find(|(t, _)| t == "assistant").unwrap();
        assert_eq!(
            http_body["_triton"]["surface"], inproc_args["_triton"]["surface"],
            "the agent is told the same surface"
        );
        assert_eq!(inproc_args["_triton"]["surface"]["channel"], "a2a");
    }

    // 2. MCP listings: the upstream tools with the widget `_meta`, and the
    //    answer-card resource.
    let mut lists = Vec::new();
    for (label, base) in hosts {
        let (status, _, tools) = rpc(&format!("{base}/mcp"), tok, "tools/list", json!({})).await;
        assert_eq!(status, 200, "{label}");
        let (_, _, resources) = rpc(&format!("{base}/mcp"), tok, "resources/list", json!({})).await;
        lists.push((
            by_name(&tools["result"]["tools"]),
            resources["result"].clone(),
        ));
    }
    assert!(
        lists[0].0.iter().any(|t| t["_meta"]["ui"].is_object()),
        "the reference lists the widget _meta: {:#?}",
        lists[0].0
    );
    assert_eq!(lists[0], lists[1], "MCP listings");

    // 3. A widget-carrying answer.
    let mut calls = Vec::new();
    for (_, base) in hosts {
        let (_, _, r) = rpc(
            &format!("{base}/mcp"),
            tok,
            "tools/call",
            json!({ "name": "answer", "arguments": { "message": "Who leads?" } }),
        )
        .await;
        calls.push(scrub(&r));
    }
    assert!(
        calls[0]["result"]["_meta"]["triton/widget"]["body"].is_array(),
        "the reference carries the widget data: {:#}",
        calls[0]
    );
    assert_eq!(calls[0], calls[1], "tools/call with the answer card");

    // 4. Error mapping: an agent fault and an unknown tool, on MCP and on
    //    the REST face.
    for (tool, what) in [("broken", "agent fault"), ("nobody", "unknown tool")] {
        let mut out = Vec::new();
        for (_, base) in hosts {
            let (_, _, mcp) = rpc(
                &format!("{base}/mcp"),
                tok,
                "tools/call",
                json!({ "name": tool, "arguments": { "message": "x" } }),
            )
            .await;
            let rest = reqwest::Client::new()
                .post(format!("{base}/v1/tools/{tool}"))
                .bearer_auth(&bearer)
                .json(&json!({ "message": "x" }))
                .send()
                .await
                .unwrap();
            let rest_status = rest.status().as_u16();
            let rest_body: Value = rest.json().await.unwrap_or(Value::Null);
            out.push(scrub(&json!({ "mcp": mcp, "rest_status": rest_status,
                                    "rest": rest_body })));
        }
        assert_eq!(out[0], out[1], "{what}");
        assert!(
            out[0]["mcp"]["error"].is_object() || out[0]["mcp"]["result"]["isError"] == true,
            "{what} is an error on MCP: {:#}",
            out[0]
        );
        assert!(out[0]["rest_status"].as_u64().unwrap() >= 400, "{what}");
    }

    // 5. The A2A cards, both versions and both mounts.
    for path in [
        "/.well-known/agent-card.json",
        "/.well-known/agent-card.json?version=0.3",
        "/a2a/.well-known/agent-card.json",
        "/.well-known/agent.json",
    ] {
        let (s0, c0) = get(&h.reference, path).await;
        let (s1, c1) = get(&h.embedded, path).await;
        assert_eq!((s0, &c0), (s1, &c1), "{path}");
        assert_eq!(s0, 200, "{path}");
    }

    // 6. The customer path: RFC 9728 metadata and the challenges.
    for (path, expected) in [
        ("/.well-known/oauth-protected-resource/mcp/acme", 200),
        ("/mcp/acme/.well-known/oauth-protected-resource", 200),
        ("/.well-known/oauth-protected-resource/mcp/initech", 404),
    ] {
        let reference = get(&h.reference, path).await;
        assert_eq!(reference.0, expected, "{path}");
        assert_eq!(reference, get(&h.embedded, path).await, "{path}");
    }
    for (token, what) in [
        (None, "no token"),
        (Some(entra_token(&h.entra, "other_scope")), "missing scope"),
        (Some(entra_token(&h.entra, "access_as_user")), "accepted"),
    ] {
        let mut out = Vec::new();
        for (_, base) in hosts {
            let (status, challenge, body) = rpc(
                &format!("{base}/mcp/acme"),
                token.as_deref(),
                "tools/list",
                json!({}),
            )
            .await;
            let body = match body["result"]["tools"].as_array() {
                Some(_) => json!(by_name(&body["result"]["tools"])),
                None => body,
            };
            out.push((status, challenge, body));
        }
        assert_eq!(out[0], out[1], "/mcp/acme, {what}");
        let expected = match what {
            "no token" => 401,
            "missing scope" => 403,
            _ => 200,
        };
        assert_eq!(out[0].0, expected, "/mcp/acme, {what}");
    }
}
