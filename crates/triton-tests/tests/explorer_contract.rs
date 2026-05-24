//! Wire-shape contract test pinning the keys the Flutter explorer's
//! REST client (`apps/explorer/lib/api/rest_client.dart`) reads from
//! Triton. If a future PR renames `tools[].input_schema` to
//! `inputSchema` or drops `returns_a2ui`, this test fails before the
//! Flutter side ever sees the mismatch.
//!
//! No mocks: real triton-bin, real HTTP.

use std::time::Duration;

use triton_tests::TritonProcess;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_v1_tools_shape_matches_dart_client() {
    let proc = TritonProcess::spawn_with(Duration::from_secs(5)).await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(proc.rest_url("/v1/tools"))
        .bearer_auth("dev-token")
        .send()
        .await
        .expect("GET /v1/tools")
        .json()
        .await
        .expect("decode JSON");

    let tools = body["tools"]
        .as_array()
        .expect("body.tools is an array; the Dart client maps from this");
    assert!(
        !tools.is_empty(),
        "expected at least one in-process tool in the registry"
    );

    // Lock the per-tool keys the SPA's ToolDescriptor.fromJson reads.
    // If any of these names move, the explorer's playground breaks
    // silently — assert here so it fails noisily instead.
    for t in tools {
        assert!(
            t["name"].is_string(),
            "tool.name must be a string for Dart: {t}"
        );
        assert!(
            t["input_schema"].is_object(),
            "tool.input_schema must be an object (snake_case key — \
             MCP camelCase is a separate surface): {t}"
        );
        assert!(
            t["returns_a2ui"].is_boolean(),
            "tool.returns_a2ui must be a bool: {t}"
        );
    }
}
