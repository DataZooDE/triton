//! v0.2 PR 36 — integration tests for the `triton-rasterizer`
//! sidecar binary.
//!
//! No mocks per CLAUDE.md §1: every test spawns the real binary,
//! talks to it over real TCP, and asserts on bytes the renderer
//! actually produced.

use std::time::Duration;

use serde_json::json;
use triton_tests::rasterizer_fixture::RasterizerProcess;

fn dashboard_body(title: &str, tiles: usize) -> serde_json::Value {
    let tiles: Vec<_> = (0..tiles)
        .map(|i| {
            json!({
                "label": format!("tile-{i}"),
                "value": format!("{}", i * 100),
                "trend": if i % 2 == 0 { Some(format!("+{i}%")) } else { None },
            })
        })
        .collect();
    json!({ "title": title, "tiles": tiles })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_post_returns_png_bytes() {
    let raster = RasterizerProcess::spawn().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let resp = client
        .post(format!("{}/v1/dashboard.png", raster.url()))
        .json(&dashboard_body("Last hour", 3))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("image/png"),
        "expected image/png content-type, got: {ct}",
    );
    let bytes = resp.bytes().await.expect("body bytes").to_vec();
    // PNG magic. Anything else and the renderer is mis-encoding.
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert!(
        bytes.len() > 200,
        "PNG body unexpectedly short ({} bytes)",
        bytes.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_dashboard_rejected_with_400() {
    let raster = RasterizerProcess::spawn().await;
    let client = reqwest::Client::new();

    // 50 tiles exceeds MAX_TILES = 32.
    let resp = client
        .post(format!("{}/v1/dashboard.png", raster.url()))
        .json(&dashboard_body("Huge", 50))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("32") || body.contains("max"),
        "expected cap message in body, got: {body}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_tile_value_rejected_with_400() {
    // PR 38 codex review hardening: a tile string past
    // MAX_TILE_FIELD_BYTES (128) must be refused at the rasterizer
    // edge BEFORE any SVG / raster work. A title-only cap would
    // miss this — the renderer's hot path scales with
    // tile-field length even when title length and tile count
    // both look healthy.
    let raster = RasterizerProcess::spawn().await;
    let client = reqwest::Client::new();

    // One tile whose `value` is 129 bytes (cap+1).
    let body = json!({
        "title": "ok",
        "tiles": [{
            "label": "a",
            "value": "x".repeat(triton_rasterizer::MAX_TILE_FIELD_BYTES + 1),
            "trend": null,
        }],
    });
    let resp = client
        .post(format!("{}/v1/dashboard.png", raster.url()))
        .json(&body)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("tile[0].value")
            && body.contains(&format!("{}", triton_rasterizer::MAX_TILE_FIELD_BYTES)),
        "expected per-field cap message, got: {body}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_body_rejected_with_400() {
    let raster = RasterizerProcess::spawn().await;
    let client = reqwest::Client::new();

    // `{}` is missing both `title` and `tiles` — serde fails at
    // deserialise time and the binary returns 400.
    let resp = client
        .post(format!("{}/v1/dashboard.png", raster.url()))
        .json(&json!({}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_dont_deadlock() {
    let raster = RasterizerProcess::spawn().await;
    let url = format!("{}/v1/dashboard.png", raster.url());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();

    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for i in 0..20 {
        let client = client.clone();
        let url = url.clone();
        let body = dashboard_body(&format!("dash-{i}"), 4);
        tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .json(&body)
                .send()
                .await
                .map(|r| r.status().as_u16())
        }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap().unwrap());
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "20 concurrent renders must complete within 10s deadline; took {:?}",
        started.elapsed(),
    );
    for s in statuses {
        assert_eq!(s, 200, "expected 200; got {s}");
    }
}

/// Issue #135 regression — end-to-end. The rendered dashboard PNG must
/// contain visible text glyphs, not just empty colored boxes. We decode
/// the PNG the real sidecar produced and count near-black opaque pixels:
/// the template draws every glyph in slate-900 and everything else is
/// light, so dark pixels are text ink. With no font loaded this was ~0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_png_contains_visible_text() {
    let raster = RasterizerProcess::spawn().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let body = json!({
        "title": "Supplier Risk",
        "tiles": [
            { "label": "Score", "value": "8 8 8 8 8", "trend": "+12%" },
            { "label": "Open findings", "value": "3", "trend": null }
        ]
    });
    let resp = client
        .post(format!("{}/v1/dashboard.png", raster.url()))
        .json(&body)
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), 200);
    let png = resp.bytes().await.expect("body bytes").to_vec();

    let pm = tiny_skia::Pixmap::decode_png(&png).expect("decode rasterizer PNG");
    let dark = pm
        .pixels()
        .iter()
        .filter(|p| p.red() < 80 && p.green() < 80 && p.blue() < 80 && p.alpha() > 200)
        .count();
    assert!(
        dark > 300,
        "rendered dashboard must contain visible text glyphs (#135); found only {dark} \
         dark text-ink pixels — empty colored boxes again?",
    );
}
