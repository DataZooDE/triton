//! A sole template form on Telegram.
//!
//! The legacy form-only interceptor turned any single-form surface into the
//! numbered-prompt state machine and finally dispatched the form's `tool` —
//! empty for a template form. A template form must instead degrade to its
//! "will be sent as" copy line (no multi-turn), and the next message the
//! user types is a NEW request routed to the agent. Real binary, real HTTP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeTelegramApi;
use triton_tests::upstream_fixture::FakeAgent;

fn update(id: u64, text: &str) -> Value {
    json!({ "update_id": id, "message": {
        "message_id": id, "from": { "id": 42, "is_bot": false, "first_name": "Alice" },
        "chat": { "id": 42, "type": "private" }, "date": 1_700_000_000, "text": text } })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sole_template_form_degrades_to_a_copy_line_not_a_form_dialogue() {
    let tg = FakeTelegramApi::start().await;
    let agent = FakeAgent::start_returning(json!({ "surface": { "components": [
        { "kind": "form", "title": "Filter agents", "submit_label": "Show", "fields": [
            { "name": "capability", "label": "Capability", "kind": "select", "required": true,
              "options": [ { "label": "Production", "value": "Production" } ] } ],
          "send_template": "Show agents in {capability}" }
    ] } }))
    .await;
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_MANIFEST_PATH".to_string(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/manifest-telegram-conversation.yaml")
                .display()
                .to_string(),
        ),
        ("TRITON_TELEGRAM_API_BASE".to_string(), tg.url()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    let triton = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let hook = triton.chat_webhook_addr.expect("webhook");
    let post = |body: Value| {
        let url = format!("http://{hook}/telegram/webhook");
        async move {
            reqwest::Client::new()
                .post(url)
                .header("X-Telegram-Bot-Api-Secret-Token", "webhook-secret-for-test")
                .json(&body)
                .send()
                .await
                .expect("POST webhook")
                .status()
        }
    };

    assert!(post(update(1, "filter please")).await.is_success());
    let start = Instant::now();
    let sent = loop {
        if let Some(m) = tg.captured().into_iter().next() {
            break m;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no reply sent");
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = sent.body["text"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("Show agents in \u{2039}Capability\u{203a}"),
        "a template form must degrade to its copy line: {text}"
    );
    assert!(!text.contains("1/1"), "no numbered-prompt dialogue: {text}");

    // The next message is a NEW request for the agent — not a form answer
    // that would dispatch the form's (empty) tool.
    assert!(
        post(update(2, "Show agents in Production"))
            .await
            .is_success()
    );
    let start = Instant::now();
    while agent.bodies_seen().len() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "second turn never reached the agent"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        agent.bodies_seen()[1]["message"],
        "Show agents in Production"
    );
}
