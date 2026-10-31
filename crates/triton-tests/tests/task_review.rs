//! Review fixes for spec A2A, inbound issuers and tasks, pinned on the real
//! binary: task ownership and resume authority (F1), the 0.3 stream under
//! broadcast lag (F4), the task deadline (F6), retention and what is
//! persisted (F5/F10), the turn context on resume (F11) and
//! `statusTimestampAfter` (F12).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};
use triton_tests::upstream_fixture::FakeAgent;
use triton_tests::{TestIssuer, TritonProcess};

const AUD: &str = "task-review-aud";

fn base_env(upstream: &str, extra: &[(&str, String)]) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ask_portfolio_agent={upstream}"),
        ),
        (
            "TRITON_A2A_PUBLIC_URL".to_string(),
            "https://agents.example.test/a2a".to_string(),
        ),
        (
            "TRITON_A2A_DEFAULT_TOOL".to_string(),
            "ask_portfolio_agent".to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k.to_string(), v.clone());
    }
    env
}

async fn spawn(env: HashMap<String, String>) -> TritonProcess {
    TritonProcess::spawn_with_env(Duration::from_secs(10), env).await
}

async fn rpc(
    t: &TritonProcess,
    token: &str,
    version: Option<&str>,
    method: &str,
    params: Value,
) -> Value {
    let mut req = reqwest::Client::new()
        .post(t.rest_url("/a2a"))
        .bearer_auth(token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    let resp = req.send().await.expect("rpc");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.unwrap();
    if ct.starts_with("text/event-stream") {
        let events: Vec<Value> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .collect();
        return json!({ "sse": events });
    }
    serde_json::from_str(&body).unwrap_or(Value::Null)
}

fn send10(text: &str, id: &str, immediately: bool) -> Value {
    json!({ "message": { "role": "ROLE_USER", "messageId": id, "parts": [{ "text": text }] },
        "configuration": { "returnImmediately": immediately } })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn tmp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "task-review-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("tasks.db")
}

/// F1: a task is its owner's. A colleague in the same data tenant (an
/// Entra tenant is a whole customer organisation) neither reads nor
/// resumes it; a named audit operator reads it but never drives it; only
/// the owner's read resumes it after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_owner_reads_and_resumes_a_task() {
    let iss = TestIssuer::start().await;
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(5),
        ..AsyncPlan::default()
    })
    .await;
    let db = tmp_db("owner");
    let env = base_env(
        &agent.host_port(),
        &[
            ("TRITON_OIDC_ISSUER", iss.issuer_url()),
            ("TRITON_OIDC_AUDIENCE", AUD.to_string()),
            ("TRITON_TASK_DB", db.display().to_string()),
        ],
    );
    let token = |sub: &str, tenant: &str, scope: &str| {
        iss.sign_jwt(json!({ "iss": iss.issuer_url(), "aud": AUD, "sub": sub,
            "tenant": tenant, "scope": scope, "exp": now() + 600 }))
    };
    let alice = token("alice", "acme", "chat");
    let bob = token("bob", "acme", "chat");
    let ops = token("ops", "ops", "audit:read-all");

    let mut first = spawn(env.clone()).await;
    let sent = rpc(
        &first,
        &alice,
        Some("1.0"),
        "SendMessage",
        send10("Propose owners", "m-o", true),
    )
    .await;
    let id = sent["result"]["task"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    tokio::time::sleep(Duration::from_millis(600)).await;
    for (method, version) in [
        ("GetTask", Some("1.0")),
        ("SubscribeToTask", Some("1.0")),
        ("tasks/get", None),
    ] {
        let got = rpc(&first, &bob, version, method, json!({ "id": id })).await;
        let err = if got["sse"].is_array() {
            got["sse"][0]["error"].clone()
        } else {
            got["error"].clone()
        };
        assert_eq!(
            err["code"], -32001,
            "a tenant-mate's {method} is not found: {got}"
        );
    }
    first.terminate(Duration::from_secs(5));
    drop(first);

    let second = spawn(env).await;
    let foreign = rpc(&second, &bob, Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert_eq!(foreign["error"]["code"], -32001, "{foreign}");
    let op = rpc(&second, &ops, Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert_eq!(op["result"]["id"], id, "the operator reads it: {op}");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        agent.resumes().is_empty(),
        "neither the colleague nor the operator resumed it: {:?}",
        agent.resumes()
    );
    let own = rpc(&second, &alice, Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert_eq!(own["result"]["id"], id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent.resumes().is_empty() {
        assert!(Instant::now() < deadline, "the owner's read resumes it");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // F11: the resume carries the turn's context, not a blank one.
    let resume = agent
        .requests()
        .into_iter()
        .find(|r| r.resume.is_some())
        .unwrap();
    let ctx = sent["result"]["task"]["contextId"].as_str().unwrap();
    assert_eq!(
        resume.body["_triton"]["a2a_context_id"], ctx,
        "{:#}",
        resume.body
    );
}

/// F6: `TRITON_TASK_MAX_SECS` is a maximum — for a stream that keeps
/// sending progress and for a dispatch that never opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_deadline_bounds_open_streams_and_stalled_dispatches() {
    let chatty = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(6),
        ..AsyncPlan::default()
    })
    .await;
    let t = spawn(base_env(
        &chatty.host_port(),
        &[
            ("TRITON_TASK_MAX_SECS", "1".to_string()),
            ("TRITON_A2A_BLOCK_SECS", "4".to_string()),
        ],
    ))
    .await;
    let started = Instant::now();
    let sent = rpc(
        &t,
        "dev-token",
        Some("1.0"),
        "SendMessage",
        send10("x", "m-d1", false),
    )
    .await;
    assert_eq!(
        sent["result"]["task"]["status"]["state"], "TASK_STATE_FAILED",
        "{sent}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(
        sent["result"]["task"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{sent}"
    );

    let stalled = FakeAgent::start_returning_after(
        Duration::from_secs(6),
        json!({ "surface": { "text": "late" } }),
    )
    .await;
    let t2 = spawn(base_env(
        &stalled.host_port(),
        &[
            ("TRITON_TASK_MAX_SECS", "1".to_string()),
            ("TRITON_A2A_BLOCK_SECS", "4".to_string()),
        ],
    ))
    .await;
    let started = Instant::now();
    let sent = rpc(
        &t2,
        "dev-token",
        Some("1.0"),
        "SendMessage",
        send10("x", "m-d2", false),
    )
    .await;
    assert_eq!(
        sent["result"]["task"]["status"]["state"], "TASK_STATE_FAILED",
        "{sent}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// F5: stored tasks expire at their TTL without a restart, and a plain
/// (synchronous) MCP call is not a task at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_expire_at_their_ttl_and_sync_mcp_calls_are_not_stored() {
    let agent = FakeAgent::start_returning(json!({ "surface": { "text": "ok" } })).await;
    let db = tmp_db("ttl");
    let t = spawn(base_env(
        &agent.host_port(),
        &[
            ("TRITON_TASK_DB", db.display().to_string()),
            ("TRITON_TASK_TTL_SECS", "1".to_string()),
        ],
    ))
    .await;
    let mcp: Value = reqwest::Client::new()
        .post(t.rest_url("/mcp"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "ask_portfolio_agent", "arguments": { "message": "hi" } } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mcp["result"]["isError"], false, "{mcp}");
    let listed = rpc(&t, "dev-token", Some("1.0"), "ListTasks", json!({})).await;
    assert_eq!(
        listed["result"]["tasks"],
        json!([]),
        "a sync MCP call is no task: {listed}"
    );

    let sent = rpc(
        &t,
        "dev-token",
        Some("1.0"),
        "SendMessage",
        send10("x", "m-ttl", false),
    )
    .await;
    let id = sent["result"]["task"]["id"].as_str().unwrap().to_string();
    let got = rpc(&t, "dev-token", Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert_eq!(got["result"]["id"], id);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let gone = rpc(&t, "dev-token", Some("1.0"), "GetTask", json!({ "id": id })).await;
    assert_eq!(gone["error"]["code"], -32001, "expired: {gone}");
    let listed = rpc(&t, "dev-token", Some("1.0"), "ListTasks", json!({})).await;
    assert_eq!(listed["result"]["tasks"], json!([]), "{listed}");
}

/// F12: `statusTimestampAfter` is a timestamp, not a string.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_timestamp_after_is_parsed() {
    let agent = FakeAgent::start_returning(json!({ "surface": { "text": "ok" } })).await;
    let t = spawn(base_env(&agent.host_port(), &[])).await;
    let sent = rpc(
        &t,
        "dev-token",
        Some("1.0"),
        "SendMessage",
        send10("x", "m-ts", false),
    )
    .await;
    let id = sent["result"]["task"]["id"].as_str().unwrap().to_string();
    let list = |after: &str| {
        let t = &t;
        let after = after.to_string();
        async move {
            rpc(
                t,
                "dev-token",
                Some("1.0"),
                "ListTasks",
                json!({ "statusTimestampAfter": after }),
            )
            .await
        }
    };
    let bad = list("yesterday").await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    let past = list("2020-01-01T00:00:00Z").await;
    assert!(
        past["result"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == id),
        "{past}"
    );
    // The same instant far in the future, with an offset: nothing after it.
    let future = list("2099-01-01T02:00:00+02:00").await;
    assert_eq!(future["result"]["tasks"], json!([]), "{future}");
}

/// F4: a 0.3 stream whose reader falls behind still ends with the whole
/// answer — the final artifact replaces what the client accumulated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_0_3_stream_still_delivers_the_whole_answer() {
    let agent = FakeAsyncAgent::start(AsyncPlan {
        tokens: Some(6000),
        ..AsyncPlan::default()
    })
    .await;
    let t = spawn(base_env(&agent.host_port(), &[])).await;
    let resp = reqwest::Client::new()
        .post(t.rest_url("/a2a"))
        .bearer_auth("dev-token")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 5, "method": "message/stream",
            "params": { "message": { "kind": "message", "role": "user", "messageId": "m-lag",
                "parts": [{ "kind": "text", "text": "long answer" }] } } }),
        )
        .send()
        .await
        .unwrap();
    // Fall behind: do not read while the upstream floods tokens.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let body = resp.text().await.unwrap();
    let mut text = String::new();
    for e in body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
    {
        let r = &e["result"];
        if r["kind"] != "artifact-update" {
            continue;
        }
        let chunk: String = r["artifact"]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect();
        if r["append"] == true {
            text.push_str(&chunk);
        } else if !chunk.is_empty() {
            text = chunk;
        }
    }
    let expected = triton_tests::async_agent_fixture::token_text(6000);
    assert_eq!(text.len(), expected.len(), "the client's text is complete");
    assert_eq!(text, expected);
}
