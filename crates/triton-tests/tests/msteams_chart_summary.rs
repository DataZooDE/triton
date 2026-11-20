//! Issue #375 (1): a Teams reply draws its native `Chart.*` from the
//! upstream's neutral chart summary (`_meta.chart`), with the deprecated
//! `_meta.vega_specs` as the fallback for one release.
//!
//! Real binary, real Bot Framework JWT against `FakeBotFramework`, a real
//! `assistant` upstream answering with an inline `report`, and a real
//! `render_report` upstream (`FakeAgent`) returning the chart result. The
//! unit tests in `triton-chat-msteams` cover every kind's mapping; this
//! covers the wire: summary → card, vega fallback, malformed summary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use triton_tests::TritonProcess;
use triton_tests::chat_courier_fixture::FakeBotFramework;
use triton_tests::upstream_fixture::FakeAgent;

const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAP8/xJ8oAAAAAElFTkSuQmCC";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn spawn(
    fake: &FakeBotFramework,
    assistant: &FakeAgent,
    report: &FakeAgent,
) -> TritonProcess {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-msteams-report-test.yaml")
        .display()
        .to_string();
    let env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        ("TRITON_MANIFEST_PATH".to_string(), manifest),
        ("TRITON_MSTEAMS_OPENID_URL".to_string(), fake.openid_url()),
        ("TRITON_MSTEAMS_TOKEN_URL".to_string(), fake.token_url()),
        (
            "TRITON_MSTEAMS_EXTRA_SERVICE_URL_HOSTS".to_string(),
            "127.0.0.1".to_string(),
        ),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!(
                "assistant={},render_report={}",
                assistant.host_port(),
                report.host_port()
            ),
        ),
        (
            "TRITON_MSTEAMS_PUBLIC_BASE".to_string(),
            "https://teams.example".to_string(),
        ),
    ]);
    TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
}

/// Post one inbound message and return the first card Triton sends back.
async fn reply_card(render_result: Value) -> Value {
    let fake = FakeBotFramework::start().await;
    let assistant = FakeAgent::start_returning(json!({
        "surface": { "components": [
            { "kind": "text", "value": "Revenue is up." },
            { "kind": "report", "report_id": "revenue", "args": { "params": {} } }
        ] }
    }))
    .await;
    let report = FakeAgent::start_returning(render_result).await;
    let proc = spawn(&fake, &assistant, &report).await;
    let webhook = proc.chat_webhook_addr.expect("chat webhook listener");
    let jwt = fake.sign_jwt(json!({
        "iss": "https://api.botframework.com",
        "aud": "triton-msteams-test-appid",
        "exp": now_unix() + 600,
        "iat": now_unix() - 5,
        "serviceurl": fake.service_url(),
    }));
    let resp = reqwest::Client::new()
        .post(format!("http://{webhook}/msteams/webhook"))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&json!({
            "type": "message", "id": "msg-1",
            "serviceUrl": "https://placeholder.example/", "channelId": "msteams",
            "from": { "id": "29:1abc", "name": "Alice" },
            "conversation": { "id": "a:conv-1", "conversationType": "personal" },
            "recipient": { "id": "28:bot-1", "name": "MyBot" },
            "text": "how is revenue?", "textFormat": "plain"
        }))
        .send()
        .await
        .expect("POST inbound");
    assert!(resp.status().is_success(), "{}", resp.status());
    let start = Instant::now();
    loop {
        if let Some(first) = fake.captured().into_iter().next() {
            return first.body["attachments"][0]["content"].clone();
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no reply posted");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn chart_element(card: &Value) -> Option<Value> {
    card["body"]
        .as_array()?
        .iter()
        .find(|e| e["type"].as_str().is_some_and(|t| t.starts_with("Chart.")))
        .cloned()
}

fn vega_specs() -> Value {
    json!([ { "mark": "bar",
        "data": { "values": [ { "k": "from-vega", "v": 3 } ] },
        "encoding": { "x": { "field": "k" }, "y": { "field": "v" } } } ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_neutral_summary_becomes_a_native_teams_chart() {
    let card = reply_card(json!({
        "content": [ { "type": "text", "text": "Revenue" } ],
        "_meta": {
            "png_base64": TINY_PNG_B64,
            "chart": {
                "kind": "bar", "title": "Revenue by month", "x": "month", "y": "revenue",
                "series": "line", "y_title": "Revenue", "unit": "EUR",
                "rows": [
                    { "month": "Jan", "revenue": 10, "line": "Widgets" },
                    { "month": "Jan", "revenue": 4, "line": "Services" },
                    { "month": "Feb", "revenue": 12, "line": "Widgets" }
                ]
            },
            "vega_specs": vega_specs()
        }
    }))
    .await;
    let chart = chart_element(&card).unwrap_or_else(|| panic!("no Chart.* element: {card}"));
    assert_eq!(chart["type"], "Chart.VerticalBar.Grouped", "{chart}");
    assert_eq!(chart["title"], "Revenue by month");
    assert_eq!(chart["yAxisTitle"], "Revenue (EUR)");
    assert_eq!(chart["data"][0]["legend"], "Widgets");
    assert_eq!(chart["data"][0]["values"][1]["x"], "Feb");
    assert_eq!(chart["data"][1]["legend"], "Services");
    // The rasterized PNG stays the fallback for hosts without charts.
    assert_eq!(chart["fallback"]["type"], "Image");
    assert!(
        chart["fallback"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("https://teams.example/msteams/img/")),
        "{chart}"
    );
    assert!(
        !card.to_string().contains("from-vega"),
        "summary wins: {card}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_summary_the_vega_specs_still_draw_the_chart() {
    let card = reply_card(json!({
        "content": [ { "type": "text", "text": "Revenue" } ],
        "_meta": { "png_base64": TINY_PNG_B64, "vega_specs": vega_specs() }
    }))
    .await;
    let chart = chart_element(&card).unwrap_or_else(|| panic!("no Chart.* element: {card}"));
    assert_eq!(chart["type"], "Chart.VerticalBar");
    assert_eq!(chart["data"][0]["x"], "from-vega");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_summary_falls_back_to_vega_then_to_the_image() {
    let bad = json!({ "kind": "bar", "x": "month", "y": "revenue",
        "rows": [ { "month": "Jan", "revenue": "lots" } ] });
    let card = reply_card(json!({
        "content": [ { "type": "text", "text": "Revenue" } ],
        "_meta": { "png_base64": TINY_PNG_B64, "chart": bad, "vega_specs": vega_specs() }
    }))
    .await;
    let chart = chart_element(&card).unwrap_or_else(|| panic!("no Chart.* element: {card}"));
    assert_eq!(chart["data"][0]["x"], "from-vega", "{chart}");

    // No vega either: the card shows the signed PNG image.
    let card = reply_card(json!({
        "content": [ { "type": "text", "text": "Revenue" } ],
        "_meta": { "png_base64": TINY_PNG_B64, "chart": bad }
    }))
    .await;
    assert!(chart_element(&card).is_none(), "{card}");
    assert!(
        card["body"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "Image"),
        "image fallback: {card}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scatter_summary_shows_the_image_not_a_misdrawn_chart() {
    let card = reply_card(json!({
        "content": [ { "type": "text", "text": "Spread" } ],
        "_meta": {
            "png_base64": TINY_PNG_B64,
            "chart": { "kind": "scatter", "x": "a", "y": "b", "rows": [ { "a": 1, "b": 2 } ] },
            "vega_specs": vega_specs()
        }
    }))
    .await;
    assert!(chart_element(&card).is_none(), "{card}");
    assert!(
        card["body"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "Image")
    );
}
