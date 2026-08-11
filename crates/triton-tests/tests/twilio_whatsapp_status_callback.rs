//! #191 PR-T6 — Twilio-WhatsApp delivery-receipt status callback.
//!
//! Twilio POSTs asynchronous delivery/read receipts to a SEPARATE
//! `StatusCallback` URL (the operator must attach `StatusCallback=<url>`
//! to the outbound send; Twilio then POSTs `MessageSid`/`MessageStatus`
//! /`ErrorCode` there as the message moves through its lifecycle —
//! confirmed against Twilio's messaging-webhooks docs before writing
//! this). Triton is stateless (G-8, no persistence), so this can't
//! "look up" the original send's audit context — instead it emits its
//! own `phase: post` audit line whose `trace_id` IS the Twilio
//! `MessageSid` (not a fresh uuid), so an operator correlates via log
//! search on that id; `status_detail` carries the static label
//! `twilio_delivery_receipt` (the dispatcher's `PostResult` requires
//! `status_detail` to be `'static`, so the dynamic MessageSid/ErrorCode
//! ride on a plain `tracing` log line instead, same as every other
//! courier's diagnostic logging). Only terminal statuses (`delivered`,
//! `read`, `failed`, `undelivered`) get an audit line; intermediate ones
//! (`queued`, `sent`, `sending`) are silently 200'd — nothing new to
//! report.
//!
//! No mocks: real binary, real HTTP, real HMAC-SHA1.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTwilioApi;

const AUTH_TOKEN: &str = "twilio-auth-token-for-test";

fn manifest_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-twilio-whatsapp-status-callback-test.yaml")
        .display()
        .to_string()
}

fn webhook_path() -> &'static str {
    "/twilio-whatsapp/webhook"
}

fn status_path() -> &'static str {
    "/twilio-whatsapp/status"
}

fn env_with(port: u16, twilio: &FakeTwilioApi) -> HashMap<String, String> {
    HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest_path()),
        ("TRITON_TWILIO_API_BASE".to_string(), twilio.url()),
        ("TRITON_CHAT_WEBHOOK_PORT".to_string(), port.to_string()),
        (
            "TRITON_TWILIO_WHATSAPP_PUBLIC_URL".to_string(),
            format!("http://127.0.0.1:{port}{}", webhook_path()),
        ),
        (
            "TRITON_TWILIO_WHATSAPP_STATUS_CALLBACK_URL".to_string(),
            format!("http://127.0.0.1:{port}{}", status_path()),
        ),
    ])
}

fn sign(url: &str, form: &[(&str, &str)], auth_token: &str) -> String {
    let mut sorted: Vec<&(&str, &str)> = form.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut s = String::from(url);
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(auth_token.as_bytes()).expect("hmac key");
    mac.update(s.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

async fn post_webhook(url: &str, form: &[(&str, &str)], signature: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(url)
        .header("X-Twilio-Signature", signature)
        .form(form)
        .send()
        .await
        .expect("POST webhook")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_send_attaches_status_callback_param() {
    // A regular inbound message triggers a post-back; the courier POST
    // Twilio receives must carry the configured StatusCallback URL so
    // Twilio knows where to send delivery receipts.
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let webhook_url = format!("{base_url}{}", webhook_path());

    let form = [
        ("From", "whatsapp:+491701234567"),
        ("To", "whatsapp:+14155238886"),
        ("Body", "hello"),
    ];
    let sig = sign(&webhook_url, &form, AUTH_TOKEN);
    let resp = post_webhook(&webhook_url, &form, &sig).await;
    assert!(resp.status().is_success());

    let captured = wait_for(Duration::from_secs(2), || {
        let v = twilio.captured();
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(
        captured[0].form.get("StatusCallback").map(String::as_str),
        Some(format!("{base_url}{}", status_path()).as_str())
    );
    let _ = proc;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivered_status_records_a_post_audit_with_message_sid() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let status_url = format!("{base_url}{}", status_path());

    let form = [
        ("MessageSid", "SM00000000000000000000000000000099"),
        ("MessageStatus", "delivered"),
        ("To", "whatsapp:+491701234567"),
        ("From", "whatsapp:+14155238886"),
    ];
    let sig = sign(&status_url, &form, AUTH_TOKEN);
    let resp = post_webhook(&status_url, &form, &sig).await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "post"
            && v["protocol"] == "messenger:twilio_whatsapp"
            && v["status_label"] == "posted"
    });
    assert_eq!(
        post_audit["trace_id"], "SM00000000000000000000000000000099",
        "trace_id must be the Twilio MessageSid so operators can correlate via log search"
    );
    assert_eq!(post_audit["status_detail"], "twilio_delivery_receipt");
    let _ = proc;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_status_records_a_dropped_post_audit() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let status_url = format!("{base_url}{}", status_path());

    let form = [
        ("MessageSid", "SM00000000000000000000000000000098"),
        ("MessageStatus", "failed"),
        ("ErrorCode", "30008"),
        ("To", "whatsapp:+491701234567"),
        ("From", "whatsapp:+14155238886"),
    ];
    let sig = sign(&status_url, &form, AUTH_TOKEN);
    let resp = post_webhook(&status_url, &form, &sig).await;
    assert!(resp.status().is_success());

    let post_audit = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "post"
            && v["protocol"] == "messenger:twilio_whatsapp"
            && v["status_label"] == "dropped"
    });
    assert_eq!(
        post_audit["trace_id"], "SM00000000000000000000000000000098",
        "trace_id must be the Twilio MessageSid"
    );
    assert_eq!(post_audit["result"], "error:provider");

    // The dynamic ErrorCode rides on a plain tracing log line (the
    // audit's status_detail is a static label, per PostResult's
    // 'static bound), so check it there instead.
    let logs = proc.stdout_snapshot();
    let has_error_code_log = logs.iter().any(|l| {
        serde_json::from_str::<Value>(l)
            .map(|v| {
                v["fields"]["message"] == "twilio delivery receipt: failed"
                    && v["fields"]["error_code"] == "30008"
            })
            .unwrap_or(false)
    });
    assert!(
        has_error_code_log,
        "expected a tracing log line naming ErrorCode 30008, stdout:\n{}",
        logs.join("\n")
    );
    let _ = proc;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intermediate_status_is_silently_acked_no_audit() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let status_url = format!("{base_url}{}", status_path());

    let form = [
        ("MessageSid", "SM00000000000000000000000000000097"),
        ("MessageStatus", "sent"),
        ("To", "whatsapp:+491701234567"),
        ("From", "whatsapp:+14155238886"),
    ];
    let sig = sign(&status_url, &form, AUTH_TOKEN);
    let resp = post_webhook(&status_url, &form, &sig).await;
    assert!(resp.status().is_success());

    std::thread::sleep(Duration::from_millis(250));
    let lines = proc.stdout_snapshot();
    let has_post_audit = lines.iter().any(|l| {
        serde_json::from_str::<Value>(l)
            .map(|v| v["kind"] == "audit" && v["phase"] == "post")
            .unwrap_or(false)
    });
    assert!(
        !has_post_audit,
        "intermediate status must not emit an audit line"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_status_callback_signature_is_rejected() {
    let port = triton_tests::free_tcp_port();
    let twilio = FakeTwilioApi::start().await;
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env_with(port, &twilio)).await;
    let base_url = format!("http://127.0.0.1:{port}");
    let status_url = format!("{base_url}{}", status_path());

    let form = [
        ("MessageSid", "SM00000000000000000000000000000096"),
        ("MessageStatus", "delivered"),
    ];
    let resp = post_webhook(&status_url, &form, "not-a-real-signature==").await;
    assert_eq!(resp.status(), 401);

    let rejected = wait_for_audit(&proc, Duration::from_secs(2), |v| {
        v["kind"] == "audit"
            && v["phase"] == "rejected"
            && v["protocol"] == "messenger:twilio_whatsapp"
    });
    assert_eq!(rejected["result"], "error:auth");
}

fn wait_for_audit<F>(proc: &TritonProcess, deadline: Duration, mut matches: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let start = Instant::now();
    loop {
        for line in proc.stdout_snapshot() {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if matches(&v) {
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
