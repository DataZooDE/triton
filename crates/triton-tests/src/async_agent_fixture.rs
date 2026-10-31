//! A fake upstream agent that speaks the asynchronous task protocol of
//! `doc/upstream-agent-contract.md` §7. No mocks: a real HTTP
//! server on a real socket, answering with real SSE.
//!
//! * A new call (`POST /` without `X-Triton-Task`) files a task and
//!   answers `text/event-stream`: `task {task_id, check_after_seconds,
//!   receipt}`, then `progress {text, step, percent}` every
//!   [`AsyncPlan::tick`], then `done {<result>}` once [`AsyncPlan::total`]
//!   has passed since the task was filed (or `error` with
//!   [`AsyncPlan::fail_with`]).
//! * A resume (`POST /` with `X-Triton-Task: <id>`) answers the same
//!   stream for that task from its current state: the latest progress
//!   first, then the rest. An unknown id is `404`.
//! * [`AsyncPlan::drop_after`] closes a stream after that many progress
//!   events without a terminal event — a broken connection.
//!
//! The task clock is the agent's, so a task keeps running while nobody
//! listens — exactly what a Triton restart has to cope with.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// How the fake agent's tasks behave.
#[derive(Clone, Debug)]
pub struct AsyncPlan {
    /// Time from filing to the terminal event.
    pub total: Duration,
    /// Interval between progress events.
    pub tick: Duration,
    /// Close each stream after this many progress events (no terminal).
    pub drop_after: Option<usize>,
    /// End with `error {message}` instead of `done`.
    pub fail_with: Option<String>,
    /// The `done` payload.
    pub result: Value,
    /// `check_after_seconds` in the `task` event.
    pub check_after_seconds: u64,
    /// The hand-over text in the `task` event.
    pub receipt: String,
    /// The working card's line under the steps, in the `task` event.
    pub note: Option<String>,
    /// Instead of a task: stream this many `token` events as fast as the
    /// reader takes them, then `done` with their whole text
    /// ([`token_text`]) — a flood for broadcast-lag tests.
    pub tokens: Option<usize>,
    /// Send only this many `progress` events, numbered 1..=n, then stay
    /// silent (stream open) until the terminal event — an upstream that
    /// reports a burst of steps and then works quietly.
    pub progress_limit: Option<usize>,
}

/// The text a [`AsyncPlan::tokens`] flood of `n` tokens adds up to.
pub fn token_text(n: usize) -> String {
    (0..n).map(|i| format!("w{i} ")).collect()
}

impl Default for AsyncPlan {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(2),
            tick: Duration::from_millis(200),
            drop_after: None,
            fail_with: None,
            result: json!({ "surface": { "components": [
                { "kind": "text", "value": "4 drafts ready for review." }
            ] } }),
            check_after_seconds: 5,
            receipt: "Your request is filed and still running.".to_string(),
            note: None,
            tokens: None,
            progress_limit: None,
        }
    }
}

/// One request the agent received.
#[derive(Clone, Debug)]
pub struct AsyncRequest {
    /// `X-Triton-Task`, for a resume.
    pub resume: Option<String>,
    pub tool: Option<String>,
    pub bearer: String,
    pub body: Value,
}

struct Shared {
    plan: AsyncPlan,
    filed: Mutex<HashMap<String, Instant>>,
    requests: Mutex<Vec<AsyncRequest>>,
    next: Mutex<u32>,
}

pub struct FakeAsyncAgent {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl FakeAsyncAgent {
    pub async fn start(plan: AsyncPlan) -> Self {
        let shared = Arc::new(Shared {
            plan,
            filed: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            next: Mutex::new(0),
        });
        let app = Router::new()
            .route("/", post(handle))
            .with_state(shared.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, shared }
    }

    pub fn host_port(&self) -> String {
        self.addr.to_string()
    }

    pub fn requests(&self) -> Vec<AsyncRequest> {
        self.shared.requests.lock().unwrap().clone()
    }

    /// Calls that filed a new task (not resumes).
    pub fn filed_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.resume.is_none())
            .count()
    }

    /// Resume calls, with the task id each named.
    pub fn resumes(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter_map(|r| r.resume)
            .collect()
    }
}

fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let resume = headers
        .get("x-triton-task")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    shared.requests.lock().unwrap().push(AsyncRequest {
        resume: resume.clone(),
        tool: headers
            .get("x-triton-tool")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        bearer: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_start_matches("Bearer ").to_string())
            .unwrap_or_default(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    let (task_id, filed_at) = match &resume {
        Some(id) => match shared.filed.lock().unwrap().get(id) {
            Some(at) => (id.clone(), *at),
            None => return (StatusCode::NOT_FOUND, "no such task").into_response(),
        },
        None => {
            let mut n = shared.next.lock().unwrap();
            *n += 1;
            let id = format!("up-{n}");
            let at = Instant::now();
            shared.filed.lock().unwrap().insert(id.clone(), at);
            (id, at)
        }
    };
    let plan = shared.plan.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    tokio::spawn(async move {
        if let Some(n) = plan.tokens {
            for i in 0..n {
                let t = json!({ "delta": format!("w{i} ") });
                if tx.send(Ok(frame("token", &t))).await.is_err() {
                    return;
                }
            }
            let done = json!({ "surface": { "text": token_text(n) } });
            let _ = tx.send(Ok(frame("done", &done))).await;
            return;
        }
        let mut task = json!({ "task_id": task_id, "check_after_seconds": plan.check_after_seconds,
            "receipt": plan.receipt });
        if let Some(note) = &plan.note {
            task["note"] = json!(note);
        }
        if tx.send(Ok(frame("task", &task))).await.is_err() {
            return;
        }
        let steps = (plan.total.as_millis() / plan.tick.as_millis().max(1)).max(1) as u64;
        let mut sent = 0usize;
        loop {
            let elapsed = filed_at.elapsed();
            if elapsed >= plan.total {
                let end = match &plan.fail_with {
                    Some(msg) => frame("error", &json!({ "message": msg })),
                    None => frame("done", &plan.result),
                };
                let _ = tx.send(Ok(end)).await;
                return;
            }
            if plan.progress_limit.is_some_and(|n| sent >= n) {
                tokio::time::sleep(plan.total.saturating_sub(elapsed)).await;
                continue;
            }
            let k = match plan.progress_limit {
                Some(_) => sent as u64 + 1,
                None => (elapsed.as_millis() / plan.tick.as_millis().max(1)) as u64 + 1,
            };
            let progress = json!({
                "text": format!("Step {k} of {steps}"),
                "step": format!("{k}/{steps}"),
                "percent": (k * 100 / (steps + 1)).min(99),
            });
            if tx.send(Ok(frame("progress", &progress))).await.is_err() {
                return;
            }
            sent += 1;
            if plan.drop_after.is_some_and(|n| sent >= n) {
                return; // close without a terminal event
            }
            tokio::time::sleep(plan.tick).await;
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}
