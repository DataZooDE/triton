//! Long-running upstream calls as tasks (upstream contract §7).
//!
//! An upstream that needs minutes answers a dispatch with `task`, then
//! `progress` events, then `done`/`error`. Triton turns that into an A2A
//! task: progress becomes working status updates (0.3 `final: false`, 1.0
//! `statusUpdate` with the step text), a blocking call hands over at the
//! cap with the upstream's receipt, the task survives a broken stream and
//! a Triton restart (SQLite task store) by RESUMING the upstream task —
//! never by filing the request a second time — and MCP answers "filed".
//!
//! No mocks: the real binary, a real SSE-speaking upstream
//! (`FakeAsyncAgent`), real sockets, a real SQLite file.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::async_agent_fixture::{AsyncPlan, FakeAsyncAgent};

use crate::a2a_schema::{check03, check10};

const PUBLIC: &str = "https://agents.example.test/a2a";
const ANSWER: &str = "4 drafts ready for review.";

fn env(agent: &FakeAsyncAgent, extra: &[(&str, &str)]) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_SINGLE_PORT".to_string(), "true".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("ask_portfolio_agent={}", agent.host_port()),
        ),
        ("TRITON_A2A_PUBLIC_URL".to_string(), PUBLIC.to_string()),
        (
            "TRITON_A2A_DEFAULT_TOOL".to_string(),
            "ask_portfolio_agent".to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    env
}

async fn spawn(env: HashMap<String, String>) -> TritonProcess {
    TritonProcess::spawn_with_env(Duration::from_secs(10), env).await
}

async fn rpc(t: &TritonProcess, version: Option<&str>, method: &str, params: Value) -> Value {
    let mut req = reqwest::Client::new()
        .post(t.rest_url("/a2a"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    req.send().await.expect("rpc").json().await.expect("json")
}

async fn sse(t: &TritonProcess, version: Option<&str>, method: &str, params: Value) -> Vec<Value> {
    let mut req = reqwest::Client::new()
        .post(t.rest_url("/a2a"))
        .bearer_auth("dev-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": method, "params": params }));
    if let Some(v) = version {
        req = req.header("A2A-Version", v);
    }
    let body = req.send().await.expect("sse").text().await.expect("body");
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

fn send10(text: &str, id: &str, immediately: bool) -> Value {
    json!({ "message": { "role": "ROLE_USER", "messageId": id, "parts": [{ "text": text }] },
        "configuration": { "returnImmediately": immediately } })
}

fn send03(text: &str, id: &str) -> Value {
    json!({ "message": { "kind": "message", "role": "user", "messageId": id,
        "parts": [{ "kind": "text", "text": text }] } })
}

async fn poll_completed(t: &TritonProcess, id: &str, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let got = rpc(t, Some("1.0"), "GetTask", json!({ "id": id })).await;
        let state = got["result"]["status"]["state"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if state == "TASK_STATE_COMPLETED" || state == "TASK_STATE_FAILED" {
            return got["result"].clone();
        }
        assert!(Instant::now() < deadline, "task {id} did not finish: {got}");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_1_0_stream_carries_the_steps_as_working_status_updates() {
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let t = spawn(env(&agent, &[])).await;
    let events = sse(
        &t,
        Some("1.0"),
        "SendStreamingMessage",
        send10("Propose owners", "m-1", false),
    )
    .await;
    for e in &events {
        check10("StreamResponse", &e["result"]);
    }
    let steps: Vec<&Value> = events
        .iter()
        .map(|e| &e["result"]["statusUpdate"])
        .filter(|u| {
            u["status"]["state"] == "TASK_STATE_WORKING" && u["status"]["message"].is_object()
        })
        .collect();
    assert!(
        steps.len() >= 2,
        "several step lines (mock 1j): {events:#?}"
    );
    assert!(
        steps[0]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Step "),
        "{:#}",
        steps[0]
    );
    assert!(steps[0]["metadata"]["step"].is_string());
    let last = &events.last().unwrap()["result"];
    assert_eq!(
        last["statusUpdate"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["result"]["artifactUpdate"]["artifact"]["parts"][0]["text"] == ANSWER)
    );
    assert_eq!(agent.filed_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_0_3_stream_carries_the_steps_as_non_final_status_updates() {
    let agent = FakeAsyncAgent::start(AsyncPlan::default()).await;
    let t = spawn(env(&agent, &[])).await;
    let events = sse(&t, None, "message/stream", send03("Propose owners", "m-2")).await;
    for e in &events {
        check03("SendStreamingMessageSuccessResponse", e);
    }
    let working: Vec<&Value> = events
        .iter()
        .filter(|e| e["result"]["kind"] == "status-update" && e["result"]["final"] == false)
        .collect();
    assert!(working.len() >= 2, "{events:#?}");
    assert_eq!(working[0]["result"]["status"]["state"], "working");
    assert!(
        working[0]["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Step ")
    );
    let last = &events.last().unwrap()["result"];
    assert_eq!(last["final"], true);
    assert_eq!(last["status"]["state"], "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocking_send_hands_over_at_the_cap_and_the_task_completes() {
    let plan = AsyncPlan {
        total: Duration::from_secs(3),
        ..AsyncPlan::default()
    };
    let agent = FakeAsyncAgent::start(plan.clone()).await;
    let t = spawn(env(&agent, &[("TRITON_A2A_BLOCK_SECS", "1")])).await;

    let started = Instant::now();
    let sent = rpc(
        &t,
        Some("1.0"),
        "SendMessage",
        send10("Propose owners", "m-3", false),
    )
    .await;
    check10("SendMessageResponse", &sent["result"]);
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING", "{sent}");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "it blocked to the cap"
    );
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"], plan.receipt,
        "the hand-over is the upstream's receipt (mock 1k)"
    );
    assert_eq!(task["metadata"]["checkAfterSeconds"], 5);

    // 0.3: the same hand-over, as a 0.3 task.
    let sent03 = rpc(&t, None, "message/send", send03("Propose owners", "m-3b")).await;
    check03("SendMessageSuccessResponse", &sent03);
    assert_eq!(sent03["result"]["kind"], "task");
    assert_eq!(sent03["result"]["status"]["state"], "working");

    let done = poll_completed(&t, task["id"].as_str().unwrap(), Duration::from_secs(10)).await;
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    assert_eq!(done["artifacts"][0]["parts"][0]["text"], ANSWER);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_upstream_stream_is_resumed_not_refiled() {
    let agent = FakeAsyncAgent::start(AsyncPlan {
        drop_after: Some(2),
        ..AsyncPlan::default()
    })
    .await;
    let t = spawn(env(&agent, &[])).await;
    let sent = rpc(
        &t,
        Some("1.0"),
        "SendMessage",
        send10("Propose owners", "m-4", true),
    )
    .await;
    let id = sent["result"]["task"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    let done = poll_completed(&t, &id, Duration::from_secs(15)).await;
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    assert_eq!(agent.filed_count(), 1, "the request was filed once");
    assert!(
        agent.resumes().iter().all(|r| r == "up-1") && !agent.resumes().is_empty(),
        "resumed by the upstream task id: {:?}",
        agent.resumes()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upstream_error_fails_the_task_with_its_message() {
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(500),
        fail_with: Some("The portfolio is locked for maintenance.".into()),
        ..AsyncPlan::default()
    })
    .await;
    let t = spawn(env(&agent, &[])).await;
    let sent = rpc(
        &t,
        Some("1.0"),
        "SendMessage",
        send10("Propose owners", "m-5", false),
    )
    .await;
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED", "{sent}");
    assert!(
        task["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("locked for maintenance"),
        "{sent}"
    );
    assert_eq!(
        agent.filed_count(),
        1,
        "an upstream error is final, not resumed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_survive_a_triton_restart_with_the_sqlite_store() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("async-tasks-{}", std::process::id()))
        .join(format!("{:x}", rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tasks.db");
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(4),
        ..AsyncPlan::default()
    })
    .await;
    let db_s = db.to_string_lossy().to_string();
    let extra = [("TRITON_TASK_DB", db_s.as_str())];

    let mut first = spawn(env(&agent, &extra)).await;
    let sent = rpc(
        &first,
        Some("1.0"),
        "SendMessage",
        send10("Propose owners", "m-6", true),
    )
    .await;
    let id = sent["result"]["task"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    // Let the upstream go asynchronous, then stop Triton mid-task.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = rpc(&first, Some("1.0"), "GetTask", json!({ "id": id })).await;
        if got["result"]["status"]["message"].is_object() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no progress before the restart: {got}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    first.terminate(Duration::from_secs(5));
    drop(first);

    let second = spawn(env(&agent, &extra)).await;
    // The task is there, still working, and SubscribeToTask follows the
    // resumed upstream task to its end.
    let events = sse(&second, Some("1.0"), "SubscribeToTask", json!({ "id": id })).await;
    for e in &events {
        check10("StreamResponse", &e["result"]);
    }
    assert_eq!(
        events[0]["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
        "{events:#?}"
    );
    assert_eq!(
        events.last().unwrap()["result"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{events:#?}"
    );
    let got = rpc(&second, None, "tasks/get", json!({ "id": id })).await;
    check03("GetTaskSuccessResponse", &got);
    assert_eq!(got["result"]["status"]["state"], "completed");
    assert_eq!(got["result"]["artifacts"][0]["parts"][0]["text"], ANSWER);
    assert_eq!(
        agent.filed_count(),
        1,
        "the restart did not file the request again"
    );
    assert!(agent.resumes().contains(&"up-1".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_ends_at_its_limit_and_resubscribe_finishes_it() {
    let agent = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_secs(3),
        ..AsyncPlan::default()
    })
    .await;
    let t = spawn(env(&agent, &[("TRITON_A2A_STREAM_MAX_SECS", "1")])).await;
    let events = sse(&t, None, "message/stream", send03("Propose owners", "m-7")).await;
    assert!(
        events.iter().all(|e| e["result"]["final"] != true),
        "the limit ends the stream before the task: {events:#?}"
    );
    let id = events[0]["result"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let t2 = Resubscriber(&t);
    let more = t2.resubscribe03(&id).await;
    for e in &more {
        check03("SendStreamingMessageSuccessResponse", e);
    }
    assert_eq!(more[0]["result"]["kind"], "task");
    let final_state = more
        .iter()
        .find(|e| e["result"]["final"] == true)
        .map(|e| e["result"]["status"]["state"].clone());
    // The resubscribe stream has the same 1 s limit; poll if it ran out.
    match final_state {
        Some(state) => assert_eq!(state, "completed"),
        None => {
            let done = poll_completed(&t, &id, Duration::from_secs(10)).await;
            assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
        }
    }
}

struct Resubscriber<'a>(&'a TritonProcess);

impl Resubscriber<'_> {
    async fn resubscribe03(&self, id: &str) -> Vec<Value> {
        sse(self.0, None, "tasks/resubscribe", json!({ "id": id })).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_answers_filed_with_check_after_seconds_when_the_upstream_goes_async() {
    let plan = AsyncPlan {
        total: Duration::from_secs(4),
        ..AsyncPlan::default()
    };
    let agent = FakeAsyncAgent::start(plan.clone()).await;
    let t = spawn(env(&agent, &[("TRITON_MCP_ASYNC_WAIT_SECS", "1")])).await;
    let call = |name: &str| {
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": name, "arguments": { "message": "Propose owners" } } })
    };
    let started = Instant::now();
    let resp: Value = reqwest::Client::new()
        .post(t.rest_url("/mcp"))
        .bearer_auth("dev-token")
        .json(&call("ask_portfolio_agent"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it did not wait for the task"
    );
    let result = &resp["result"];
    assert_eq!(result["isError"], false, "{resp}");
    assert_eq!(result["structuredContent"]["status"], "filed", "{resp}");
    assert_eq!(result["structuredContent"]["check_after_seconds"], 5);
    assert_eq!(
        result["structuredContent"]["task_id"], "up-1",
        "the agent's own task id, for its status tool: {resp}"
    );
    assert!(result["_meta"]["trace_id"].is_string(), "{resp}");
    assert_eq!(result["content"][0]["text"], plan.receipt);

    // A quick asynchronous upstream is simply answered.
    let quick = FakeAsyncAgent::start(AsyncPlan {
        total: Duration::from_millis(300),
        ..AsyncPlan::default()
    })
    .await;
    let t2 = spawn(env(&quick, &[("TRITON_MCP_ASYNC_WAIT_SECS", "5")])).await;
    let resp: Value = reqwest::Client::new()
        .post(t2.rest_url("/mcp"))
        .bearer_auth("dev-token")
        .json(&call("ask_portfolio_agent"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["result"]["content"][0]["text"], ANSWER, "{resp}");
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
