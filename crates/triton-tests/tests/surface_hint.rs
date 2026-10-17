//! #352: Triton tells the upstream agent which channel a turn will be rendered
//! on (`X-Triton-Channel`). The hint is non-authoritative rendering advice;
//! the mapper still degrades whatever comes back.
//!
//! No mocks: a real spawned `triton`, a real recording upstream (`FakeAgent`)
//! and real inbound requests over TCP.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::json;
use triton_tests::{TritonProcess, upstream_fixture::FakeAgent};

fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        if start.elapsed() > deadline {
            panic!("condition not met within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn `triton` fronting `agent` as tool `assistant`, plus `extra` env.
async fn triton_with(agent: &FakeAgent, extra: &[(&str, String)]) -> TritonProcess {
    let mut env = HashMap::from([
        ("TRITON_ENV".to_string(), "local".to_string()),
        (
            "TRITON_STATIC_UPSTREAMS".to_string(),
            format!("assistant={}", agent.host_port()),
        ),
    ]);
    for (k, v) in extra {
        env.insert((*k).to_string(), v.clone());
    }
    TritonProcess::spawn_with_env(Duration::from_secs(5), env).await
}

/// The channel header of the agent's first recorded request.
fn first_channel(agent: &FakeAgent) -> Option<String> {
    wait_for(Duration::from_secs(5), || {
        let v = agent.channels_seen();
        (!v.is_empty()).then_some(v)
    })
    .remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_dispatch_carries_channel_header() {
    let agent = FakeAgent::start_echoing().await;
    let triton = triton_with(&agent, &[]).await;

    let resp = reqwest::Client::new()
        .post(triton.rest_url("/v1/tools/assistant"))
        .bearer_auth("dev-token")
        .json(&json!({ "message": "hi" }))
        .send()
        .await
        .expect("POST /v1/tools/assistant");
    assert!(resp.status().is_success(), "{}", resp.status());

    assert_eq!(first_channel(&agent).as_deref(), Some("rest"));
}
