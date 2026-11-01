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

/// Cap buffered body size (dev convenience). A request declaring a larger
/// body passes through uncaptured, so the route's own limit (checked after
/// authentication) decides; a larger response is not captured either.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Longest string value kept under a file-content key in a captured body.
const MAX_CAPTURED_FILE_CHARS: usize = 256;

/// Wrap `router` so every call's request + response JSON is captured per
/// `trace_id`. Apply once to the composed (REST + /mcp + /a2a) router.
pub fn apply(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(capture))
}

async fn capture(req: Request, next: Next) -> Response {
    let protocol = protocol_of(req.uri().path());

    let declared = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_none_or(|n| n > MAX_BODY) {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let req_bytes = to_bytes(body, MAX_BODY).await.unwrap_or_default();
    let mut req_json: Value = serde_json::from_slice(&req_bytes).unwrap_or(Value::Null);
    redact_file_content(&mut req_json);
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

/// Replace long file content (A2A `bytes`/`raw`, FileUpload `inlineData`)
/// with its length, so the trace ring never holds a customer document.
fn redact_file_content(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let long = child
                    .as_str()
                    .filter(|s| s.len() > MAX_CAPTURED_FILE_CHARS)
                    .map(str::len);
                match long {
                    Some(n) if matches!(k.as_str(), "bytes" | "raw" | "inlineData") => {
                        *child = Value::String(format!("<{n} chars>"));
                    }
                    _ => redact_file_content(child),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_file_content),
        _ => {}
    }
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

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn long_file_content_is_replaced_by_its_length() {
        let long = "A".repeat(300);
        let mut v = serde_json::json!({ "params": { "message": { "parts": [
            { "kind": "file", "file": { "bytes": long, "name": "a.txt" } },
            { "raw": "short" },
            { "data": { "action": { "context": { "files": [ { "inlineData": long } ] } } } }
        ] } } });
        redact_file_content(&mut v);
        let parts = &v["params"]["message"]["parts"];
        assert_eq!(parts[0]["file"]["bytes"], "<300 chars>");
        assert_eq!(parts[0]["file"]["name"], "a.txt");
        assert_eq!(parts[1]["raw"], "short", "short values are kept");
        assert_eq!(
            parts[2]["data"]["action"]["context"]["files"][0]["inlineData"],
            "<300 chars>"
        );
    }
}
