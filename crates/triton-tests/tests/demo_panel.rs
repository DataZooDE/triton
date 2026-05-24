//! `demo_panel` tool — emits an A2UI Surface using every component
//! variant so the explorer can render the full v0.8/v0.9 vocabulary.
//! This test pins the wire shape for both versions so a future
//! schema change has to update both sides explicitly.
//!
//! No mocks: real spawned triton-bin, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_panel_emits_full_vocabulary_in_both_versions() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let client = reqwest::Client::new();

    // v0.9 — flat, lowercase typed nodes.
    let body: serde_json::Value = client
        .post(proc.rest_url("/v1/tools/demo_panel"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=0.9")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST demo_panel v0.9")
        .json()
        .await
        .expect("decode JSON");
    let env = &body["result"];
    assert_eq!(env["version"], "0.9", "envelope shape: {body}");
    let stream = env["stream"].as_array().expect("stream array");
    let kinds: Vec<&str> = stream
        .iter()
        .map(|n| n["type"].as_str().unwrap_or(""))
        .collect();
    for k in [
        "text",
        "narration",
        "dashboard",
        "selection",
        "form",
        "button",
    ] {
        assert!(kinds.contains(&k), "v0.9 missing {k}: kinds={kinds:?}");
    }
    let sel = stream.iter().find(|n| n["type"] == "selection").unwrap();
    assert!(sel["options"].is_array());
    assert!(sel["args_key"].is_string());
    let form = stream.iter().find(|n| n["type"] == "form").unwrap();
    assert!(form["fields"].is_array());
    let dash = stream.iter().find(|n| n["type"] == "dashboard").unwrap();
    assert!(dash["tiles"].is_array());

    // v0.8 — PascalCase Component wrapper.
    let body: serde_json::Value = client
        .post(proc.rest_url("/v1/tools/demo_panel"))
        .bearer_auth("dev-token")
        .header("Accept", "application/json+a2ui; version=0.8")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST demo_panel v0.8")
        .json()
        .await
        .expect("decode JSON");
    let env = &body["result"];
    assert_eq!(env["version"], "0.8");
    let stream = env["stream"].as_array().expect("stream array");
    let outer_keys: Vec<String> = stream
        .iter()
        .filter_map(|n| {
            n.get("Component")?
                .as_object()?
                .keys()
                .next()
                .map(String::from)
        })
        .collect();
    for k in [
        "Text",
        "Narration",
        "Dashboard",
        "Selection",
        "Form",
        "Button",
    ] {
        assert!(
            outer_keys.iter().any(|x| x == k),
            "v0.8 missing {k}: kinds={outer_keys:?}"
        );
    }
}
