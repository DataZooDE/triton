//! #284 — the authorization decision Triton already makes, enforced.
//!
//! Google Chat's `self_enrol` strategy admits an unknown sender with
//! `scopes == ["pairing"]` and `tenant == "pairing"` so an enrolment
//! tool can issue a code (M-ENROL-1). Triton then **discards** that
//! distinction: the pairing principal reaches the dispatcher like any
//! other and can invoke whatever the adapter routes to, including any
//! tool a command prefix names. A sender who has not been enrolled by
//! an operator is, in practice, fully authorized.
//!
//! It is also the only authorization decision Triton makes anywhere —
//! grep the workspace and the sole scope gate is
//! `REQUIRED_OUTBOUND_SCOPE` on `/v1/outbound`. So this is both a real
//! hole and the first customer for a general seam.
//!
//! The seam is `Dispatcher::can_invoke`, default-allow: a deployment
//! that names no pairing tool behaves exactly as before, and the rule
//! only bites where an operator has declared one.
//!
//! No mocks per CLAUDE.md §1: real binary, real Google-signed OIDC
//! bearer from the in-repo JWKS fixture.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeGoogleJwks;

const AUDIENCE: &str = "1234567890";
/// Enrolled in the fixture's `fallback_table`.
const ENROLLED: &str = "users/77";
/// Not enrolled — first contact, so `scopes == ["pairing"]`.
const UNKNOWN: &str = "users/55";

fn manifest(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("fixtures/{name}"))
        .display()
        .to_string()
}

fn env_with(jwks: &FakeGoogleJwks, fixture: &str) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest(fixture)),
        ("TRITON_GOOGLE_CHAT_JWKS_URI".to_string(), jwks.jwks_uri()),
    ])
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn claims() -> Value {
    json!({
        "iss": "chat@system.gserviceaccount.com",
        "aud": AUDIENCE,
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
    })
}

/// A Chat MESSAGE event from `sender` whose text routes to a tool.
fn message_event(sender: &str, text: &str) -> Value {
    json!({
        "type": "MESSAGE",
        "eventTime": "2026-09-05T10:00:00Z",
        "space": { "name": "spaces/AAA", "type": "ROOM" },
        "message": {
            "name": "spaces/AAA/messages/1",
            "sender": { "name": sender, "type": "HUMAN" },
            "text": text,
        },
        "user": { "name": sender }
    })
}

async fn post(proc: &TritonProcess, jwks: &FakeGoogleJwks, sender: &str, text: &str) -> u16 {
    let webhook = proc.chat_webhook_addr.expect("listener bound");
    let jwt = jwks.sign_jwt(claims());
    reqwest::Client::new()
        .post(format!("http://{webhook}/google_chat/webhook"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&message_event(sender, text))
        .send()
        .await
        .expect("POST webhook")
        .status()
        .as_u16()
}

fn dispatches(proc: &TritonProcess, tool: &str) -> usize {
    proc.stdout_snapshot()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        // `result == "ok"` matters: a REFUSED dispatch is also audited
        // with `phase: dispatch` (carrying `error:forbidden`), so counting
        // the phase alone would count the refusal as a run.
        .filter(|v| {
            v["kind"] == "audit"
                && v["phase"] == "dispatch"
                && v["tool"] == tool
                && v["result"] == "ok"
        })
        .count()
}

fn wait_for_audit(proc: &TritonProcess, deadline: Duration, m: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if m(&v) {
                return v;
            }
        }
        if start.elapsed() > deadline {
            panic!(
                "audit line not found within {deadline:?}\nstdout:\n{}",
                proc.stdout_snapshot().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The hole: an unenrolled sender reaches a tool that is not the
/// enrolment tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pairing_principal_cannot_reach_an_ordinary_tool() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&jwks, "manifest-googlechat-pairing-test.yaml"),
    )
    .await;

    // `/narrate` is a routed command naming a tool that is NOT the
    // declared pairing tool. (`/echo` is not routed in google_chat — it
    // would fall through to the default and prove nothing.)
    let _ = post(&proc, &jwks, UNKNOWN, "/narrate alice").await;

    // Match the refusal for THIS tool specifically — an unrelated
    // failure (the `get_theme` chrome fetch has no upstream here) would
    // otherwise satisfy a looser predicate and the test would pass for
    // the wrong reason.
    let refused = wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit"
            && v["tool"] == "narrate"
            && v["result"]
                .as_str()
                .is_some_and(|r| r.starts_with("error:"))
    });
    assert_eq!(refused["subject"], UNKNOWN, "got: {refused}");
    assert_eq!(
        dispatches(&proc, "narrate"),
        0,
        "a pairing-scoped principal must never actually RUN an ordinary tool"
    );
}

/// ...but it must still reach the enrolment tool, or nobody can ever
/// enrol and the strategy is useless.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pairing_principal_can_still_reach_the_pairing_tool() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&jwks, "manifest-googlechat-pairing-test.yaml"),
    )
    .await;

    // Plain text routes to the adapter's configured tool, which this
    // fixture sets to the enrolment tool.
    let _ = post(&proc, &jwks, UNKNOWN, "please enrol me").await;
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "pair"
    });
}

/// An enrolled sender is unaffected — the rule keys on holding ONLY the
/// pairing scope, not on the tool being ordinary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enrolled_sender_is_unaffected() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&jwks, "manifest-googlechat-pairing-test.yaml"),
    )
    .await;

    let _ = post(&proc, &jwks, ENROLLED, "/narrate alice").await;
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
}

/// Default-allow: a deployment that names no pairing tool keeps today's
/// behaviour exactly, so this ships without breaking anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_declared_pairing_tool_nothing_changes() {
    let jwks = FakeGoogleJwks::start().await;
    let proc = TritonProcess::spawn_with_env(
        Duration::from_secs(5),
        env_with(&jwks, "manifest-googlechat-selfenrol-test.yaml"),
    )
    .await;

    let _ = post(&proc, &jwks, UNKNOWN, "/narrate alice").await;
    wait_for_audit(&proc, Duration::from_secs(5), |v| {
        v["kind"] == "audit" && v["phase"] == "dispatch" && v["tool"] == "narrate"
    });
}
