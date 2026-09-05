//! Issue #249 — coalesce pre-auth rejection audit on public routes.
//!
//! A public inbound path (`/v1/tools/*`, a chat webhook, `/api/messages`)
//! answers an unauthenticated request with 401 *and* emits one
//! `phase: rejected` audit line per request, before any rate-limit token
//! is consumed — the bucket is deliberately taken only after auth so a
//! sprayer can't burn it. A background scanner therefore writes one audit
//! line per probe, and the in-process ring buffer
//! (`AUDIT_BUFFER_CAPACITY = 1024`) evicts every real entry within
//! minutes — the history an operator tails at `/v1/audit` is gone exactly
//! when they need it.
//!
//! The fix coalesces *anonymous* rejections into one line per window per
//! protocol, carrying the count it stands for. Three properties matter
//! and are pinned here:
//!
//!   * the response is unchanged — still 401, every single time;
//!   * an IDENTIFIED subject being refused is never coalesced (that is
//!     the security signal, not the noise);
//!   * the first rejection still emits IMMEDIATELY, with its
//!     `error_detail` — #219 exists because refusals were undiagnosable,
//!     and making an operator wait out a window to see why their bot 401s
//!     would regress exactly that.
//!
//! No mocks per CLAUDE.md §1: real binary over real TCP, real identity
//! boundary, audit read back off the process's real stdout and its real
//! `/v1/audit` endpoint.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use triton_tests::TritonProcess;

/// Short window so the tests are fast and deterministic rather than
/// sleeping out the 60s production default.
const TEST_WINDOW_SECS: &str = "1";

fn env_with_window(secs: &str) -> HashMap<String, String> {
    HashMap::from([(
        "TRITON_AUDIT_REJECT_WINDOW_SECS".to_string(),
        secs.to_string(),
    )])
}

/// Every `phase: rejected` audit line the process has emitted.
fn rejection_lines(proc: &TritonProcess) -> Vec<Value> {
    proc.stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "audit" && v["phase"] == "rejected")
        .collect()
}

/// Poll until `probe` returns `Some`, so an assertion never races the
/// audit line reaching stdout.
/// #249 was filed against the msteams webhook — the public path already
/// on `agent-lab`, whose canonical `/api/messages` (#248) is the
/// heavily-scanned one. It inherits the fix through the dispatcher pivot
/// (ADR-6) with no adapter code change at all, which is the whole reason
/// the fix lives there. This drives the real Teams webhook to prove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_msteams_webhook_inherits_the_coalescing() {
    const PROBES: usize = 10;
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-test.yaml")
        .display()
        .to_string();
    let mut env = env_with_window("60");
    env.insert("TRITON_ENV".to_string(), "local".to_string());
    env.insert("TRITON_MANIFEST_PATH".to_string(), manifest);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let client = reqwest::Client::new();

    // A scanner: no bearer, straight at the public webhook.
    for i in 0..PROBES {
        let resp = client
            .post(format!("http://{webhook}/msteams/webhook"))
            .json(&serde_json::json!({ "type": "message", "text": "probe" }))
            .send()
            .await
            .expect("POST probe");
        assert_eq!(resp.status(), 401, "probe {i} must still be refused");
    }

    let lines = wait_for(Duration::from_secs(3), || {
        let l: Vec<Value> = rejection_lines(&proc)
            .into_iter()
            .filter(|v| v["protocol"] == "messenger:msteams")
            .collect();
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(
        lines.len(),
        1,
        "{PROBES} unauthenticated webhook probes must collapse to one \
         line; got {}",
        lines.len()
    );
}

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("probe did not return Some within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// N unauthenticated probes ⇒ N × 401, but ONE audit line carrying the
/// count of the ones it stands for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_rejections_coalesce_into_one_audit_line() {
    const PROBES: usize = 12;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window("60")).await;
    let client = reqwest::Client::new();

    for i in 0..PROBES {
        let resp = client
            .post(proc.rest_url("/v1/tools/echo"))
            .json(&serde_json::json!({ "message": format!("probe {i}") }))
            .send()
            .await
            .expect("POST probe");
        assert_eq!(
            resp.status(),
            401,
            "every probe must still be refused — coalescing changes the \
             audit line, never the answer (probe {i})"
        );
    }

    let lines = wait_for(Duration::from_secs(3), || {
        let l = rejection_lines(&proc);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(
        lines.len(),
        1,
        "{PROBES} anonymous probes must collapse to one audit line; got {}:\n{}",
        lines.len(),
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The surviving line is the FIRST probe's, emitted at once and
    // therefore standing for nothing yet — the count of the 11 swallowed
    // behind it lands on the next emission once the window lapses (see
    // `the_window_reopens_and_reports_what_it_swallowed`). The exact
    // total is never lost regardless: metrics count every rejection
    // unconditionally (see `metrics_count_every_rejection`).
    assert!(
        lines[0].get("suppressed").is_none(),
        "the immediate first line swallowed nothing; got: {}",
        lines[0]
    );

    // The endpoint the issue is actually about: the ring buffer an
    // operator tails is no longer flooded.
    let audit: Value = client
        .get(proc.rest_url("/v1/audit"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/audit")
        .json()
        .await
        .expect("decode audit");
    let rejected: Vec<&Value> = audit["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["phase"] == "rejected")
        .collect();
    assert_eq!(
        rejected.len(),
        1,
        "the /v1/audit tail must hold one coalesced entry, not {PROBES}"
    );
}

/// The coalescing key must be bounded by CONFIGURATION, not by the
/// request. The REST adapter audits a pre-auth rejection under
/// `Path(name)` — the URL segment an unauthenticated caller picks — and
/// MCP under a name off the JSON-RPC body. Keying the window on the tool
/// name would therefore let `/v1/tools/<random>` mint an unbounded number
/// of windows: a memory-growth DoS introduced by the very fix meant to
/// blunt a scanner. Keying on the protocol (a closed set) is what makes
/// it bounded, and this is the test that says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_attacker_chosen_paths_do_not_multiply_audit_lines() {
    const PROBES: usize = 12;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window("60")).await;
    let client = reqwest::Client::new();

    // Every probe targets a DIFFERENT tool name, as a scanner walking a
    // wordlist would.
    for i in 0..PROBES {
        let resp = client
            .post(proc.rest_url(&format!("/v1/tools/scan-{i}")))
            .json(&serde_json::json!({ "message": "x" }))
            .send()
            .await
            .expect("POST probe");
        assert_eq!(resp.status(), 401, "probe {i} must still be refused");
    }

    let lines = wait_for(Duration::from_secs(3), || {
        let l = rejection_lines(&proc);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(
        lines.len(),
        1,
        "{PROBES} distinct caller-chosen paths must still collapse to ONE          line — one window per protocol, not per tool name; got {}:\n{}",
        lines.len(),
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// An IDENTIFIED subject being refused is the security signal, not the
/// noise. It must never be swallowed — least of all by an anonymous
/// scanner sharing its protocol, which is exactly what would let a flood
/// mask a real caller's repeated refusals. Both halves here run on the
/// SAME protocol (`a2a`) so the window they'd share is the same one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identified_rejections_are_never_coalesced() {
    const CALLS: usize = 6;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window("3600")).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/message:send", proc.a2a_addr);

    // Anonymous noise first, on `a2a`, to open that protocol's window.
    for _ in 0..4 {
        let _ = client
            .post(&url)
            .json(&serde_json::json!({ "parts": [] }))
            .send()
            .await
            .expect("POST anon");
    }

    // Now an AUTHENTICATED caller who keeps getting refused: a valid
    // bearer, a Message with no Part (FR-A-7). These rejections carry a
    // real subject, so none of them may be coalesced.
    for i in 0..CALLS {
        let resp = client
            .post(&url)
            .bearer_auth("dev-token")
            .json(&serde_json::json!({ "parts": [], "n": i }))
            .send()
            .await
            .expect("POST identified");
        assert!(
            resp.status().is_client_error() || resp.status().is_success(),
            "got {}",
            resp.status()
        );
    }

    let identified = wait_for(Duration::from_secs(5), || {
        let l: Vec<Value> = rejection_lines(&proc)
            .into_iter()
            .filter(|v| v["protocol"] == "a2a" && v["subject"] != "-")
            .collect();
        (l.len() >= CALLS).then_some(l)
    });
    assert_eq!(
        identified.len(),
        CALLS,
        "every identified refusal is audited, though {} anonymous ones on \
         the same protocol were coalesced first; got {}",
        4,
        identified.len()
    );
    assert!(
        identified.iter().all(|l| l.get("suppressed").is_none()),
        "an identified refusal is never coalesced; got: {identified:?}"
    );
}

/// Coalescing costs no observability: the METRIC counts every rejection
/// at full per-tool granularity, unconditionally, before the audit line
/// is even considered. The count is what a dashboard needs; the line is
/// what floods the buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_count_every_rejection_even_when_the_line_is_coalesced() {
    const PROBES: usize = 9;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window("60")).await;
    let client = reqwest::Client::new();

    for _ in 0..PROBES {
        let _ = client
            .post(proc.rest_url("/v1/tools/echo"))
            .send()
            .await
            .expect("POST probe");
    }
    let lines = wait_for(Duration::from_secs(3), || {
        let l = rejection_lines(&proc);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(lines.len(), 1, "one audit line");

    let metrics = client
        .get(proc.rest_url("/v1/metrics"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET metrics")
        .text()
        .await
        .expect("metrics body");
    let rejected: f64 = metrics
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains("rejected"))
        .filter_map(|l| {
            l.rsplit(' ')
                .next()
                .and_then(|v| v.trim().parse::<f64>().ok())
        })
        .sum();
    assert!(
        rejected >= PROBES as f64,
        "metrics must count all {PROBES} rejections though only 1 line was \
         emitted; got {rejected}\n{metrics}"
    );
}

/// The very first refusal is audited at once, with the reason. #219 made
/// a refusal say WHY in the audit line itself; a window that delayed the
/// first line would undo that for every operator debugging a 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_rejection_is_audited_immediately_with_its_reason() {
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window("3600")).await;

    let resp = reqwest::Client::new()
        .post(proc.rest_url("/v1/tools/echo"))
        .bearer_auth("not-a-real-token")
        .json(&serde_json::json!({ "message": "x" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 401);

    // Even with an hour-long window, this line must be here now.
    let lines = wait_for(Duration::from_secs(3), || {
        let l = rejection_lines(&proc);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0]["error_detail"].is_string(),
        "the first line still carries WHY (#219); got: {}",
        lines[0]
    );
    assert!(
        lines[0].get("suppressed").is_none(),
        "nothing was suppressed yet, so the field is absent and the line \
         stays byte-identical for existing consumers; got: {}",
        lines[0]
    );
}

/// The window reopens: after it elapses, the next anonymous rejection
/// emits again, carrying the count accumulated while it was closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_reopens_and_reports_what_it_swallowed() {
    let proc =
        TritonProcess::spawn_with_env(Duration::from_secs(5), env_with_window(TEST_WINDOW_SECS))
            .await;
    let client = reqwest::Client::new();

    // First burst: one line (immediate) + 3 suppressed.
    for _ in 0..4 {
        let _ = client
            .post(proc.rest_url("/v1/tools/echo"))
            .send()
            .await
            .expect("POST");
    }
    wait_for(Duration::from_secs(3), || {
        (!rejection_lines(&proc).is_empty()).then_some(())
    });

    // Let the window lapse, then probe once more.
    std::thread::sleep(Duration::from_millis(1300));
    let _ = client
        .post(proc.rest_url("/v1/tools/echo"))
        .send()
        .await
        .expect("POST after window");

    let lines = wait_for(Duration::from_secs(3), || {
        let l = rejection_lines(&proc);
        (l.len() >= 2).then_some(l)
    });
    assert_eq!(lines.len(), 2, "a reopened window emits again");
    assert_eq!(
        lines[1]["suppressed"],
        Value::from(3),
        "the second line reports the 3 swallowed during the closed \
         window; got: {}",
        lines[1]
    );
}
