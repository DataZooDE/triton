//! Dev-only request/response **body** capture for the Explorer Trace view
//! (issue #75). Feature `capture` (off by default, never in release).
//!
//! An axum middleware buffers each call's request and response JSON and
//! records them keyed by the `trace_id` found in the response, so the
//! Trace page can show the payload at each hop. It captures **bodies
//! only** — the `Authorization` header is never read here (FR-AU-3). The
//! protocol is inferred from the path (`/mcp*` → mcp, `/a2a*` → a2a, else
//! rest), so one layer on the composed router covers the whole trio.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

/// Cap buffered body size (dev convenience; large payloads are truncated
/// to "not captured" rather than buffered unbounded).
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Wrap `router` so every call's request + response JSON is captured per
/// `trace_id`. Apply once to the composed (REST + /mcp + /a2a) router.
pub fn apply(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(capture))
}

async fn capture(req: Request, next: Next) -> Response {
    let protocol = protocol_of(req.uri().path());

    let (parts, body) = req.into_parts();
    let req_bytes = to_bytes(body, MAX_BODY).await.unwrap_or_default();
    let req_json: Value = serde_json::from_slice(&req_bytes).unwrap_or(Value::Null);
    let req = Request::from_parts(parts, Body::from(req_bytes));

    let resp = next.run(req).await;

    // Never buffer a streaming (SSE) response (TS-04): `to_bytes` would
    // hold the whole body until the stream closes, defeating incremental
    // delivery (issue #132). Pass it through untouched — the trace_id
    // lives in the terminal frame, not a top-level field, so there's
    // nothing to key a capture on anyway.
    if is_event_stream(&resp) {
        return resp;
    }

    let (rparts, rbody) = resp.into_parts();
    let resp_bytes = to_bytes(rbody, MAX_BODY).await.unwrap_or_default();
    let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap_or(Value::Null);

    if let Some(trace_id) = trace_id_of(&resp_json, protocol) {
        triton_core::trace::record(&trace_id, protocol, "request", req_json);
        triton_core::trace::record(&trace_id, protocol, "response", resp_json);
    }

    Response::from_parts(rparts, Body::from(resp_bytes))
}

/// True when the response is an SSE stream that must not be buffered.
fn is_event_stream(resp: &Response) -> bool {
    resp.headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"))
}

fn protocol_of(path: &str) -> &'static str {
    if path.starts_with("/mcp") {
        "mcp"
    } else if path.starts_with("/a2a") {
        "a2a"
    } else {
        "rest"
    }
}

/// Where each protocol's response carries the shared trace_id.
fn trace_id_of(resp: &Value, protocol: &str) -> Option<String> {
    let v = match protocol {
        "mcp" => resp
            .get("result")
            .and_then(|r| r.get("_meta"))
            .and_then(|m| m.get("trace_id")),
        "a2a" => resp.get("metadata").and_then(|m| m.get("trace_id")),
        _ => resp.get("trace_id"),
    };
    v.and_then(Value::as_str).map(String::from)
}
