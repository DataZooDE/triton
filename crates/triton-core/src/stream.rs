//! The typed streaming-event vocabulary for the SSE response path
//! (issue #132).
//!
//! A streaming tool invocation produces a sequence of [`StreamEvent`]s.
//! Two of them are *terminal* (`Done`, `Error`) and close the stream;
//! the dispatcher keys its single ADR-6 audit line off whichever
//! terminal event (or client disconnect) arrives first.
//!
//! Per ADR-4 the wire vocabulary is closed and typed: a new schema
//! version adds a variant, it never branches on `if version ==`.
//! Decoding an *unknown* upstream event name is deliberately lenient —
//! it maps to [`StreamEvent::Tool`] so a newer upstream emitting an
//! event Triton doesn't recognise is relayed rather than rejected.

use serde_json::{Value, json};

use crate::error::TritonError;

/// One event in a streaming tool invocation.
///
/// `Done` and `Error` are terminal: nothing follows them on the stream.
#[derive(Debug)]
pub enum StreamEvent {
    /// Opaque tool-progress payload, e.g. `{"step":"search","hits":30}`.
    /// Triton does not interpret the body.
    Tool(Value),
    /// An incremental token delta from the upstream LLM.
    Token(String),
    /// Terminal success — carries the tool result (or, after the A2UI
    /// layer wraps it, the built envelope).
    Done(Value),
    /// Terminal failure raised *after* the stream opened (200 headers
    /// already flushed). Surfaced to the client as a terminal `error`
    /// frame and reflected in the single audit line.
    Error(TritonError),
}

impl StreamEvent {
    /// The SSE `event:` name for this variant.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Tool(_) => "tool",
            Self::Token(_) => "token",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// True for the two stream-closing variants.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    /// The `data:` payload as a JSON value.
    pub fn data(&self) -> Value {
        match self {
            Self::Tool(v) | Self::Done(v) => v.clone(),
            Self::Token(delta) => json!({ "delta": delta }),
            Self::Error(e) => json!({ "error": e.class(), "message": e.to_string() }),
        }
    }

    /// Encode to a full SSE wire frame: `event: <name>\ndata: <json>\n\n`.
    /// The payload is compact (single-line) JSON, so one `data:` line
    /// always suffices.
    pub fn to_sse_frame(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.event_name(), self.data())
    }

    /// Parse one upstream SSE frame (its `event:` name + raw `data:`
    /// payload) into a typed event.
    ///
    /// Unknown event names map to [`StreamEvent::Tool`] for
    /// forward-compatibility — Triton never errors on an unrecognised
    /// name (issue #132).
    pub fn parse(event: &str, data: &str) -> Self {
        match event {
            "token" => {
                // Canonical shape is `{"delta":"..."}`; tolerate a bare
                // string payload too.
                let delta = serde_json::from_str::<Value>(data)
                    .ok()
                    .and_then(|v| v.get("delta").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| data.to_string());
                Self::Token(delta)
            }
            "done" => Self::Done(parse_value(data)),
            "error" => {
                // An upstream error frame is, from Triton's vantage, a
                // Tool fault regardless of the upstream's own class.
                let msg = parse_value(data)
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| data.to_string());
                Self::Error(TritonError::Tool(msg))
            }
            // "tool" and any unrecognised name → opaque progress.
            _ => Self::Tool(parse_value(data)),
        }
    }
}

/// Parse a `data:` payload as JSON, falling back to a JSON string if it
/// is not valid JSON.
fn parse_value(data: &str) -> Value {
    serde_json::from_str(data).unwrap_or_else(|_| Value::String(data.to_string()))
}

#[cfg(test)]
mod tests {
    use super::StreamEvent;
    use crate::error::TritonError;
    use serde_json::json;

    #[test]
    fn tool_frame_carries_opaque_payload() {
        let ev = StreamEvent::Tool(json!({ "step": "search", "hits": 30 }));
        assert_eq!(
            ev.to_sse_frame(),
            "event: tool\ndata: {\"hits\":30,\"step\":\"search\"}\n\n"
        );
        assert!(!ev.is_terminal());
    }

    #[test]
    fn token_frame_wraps_delta() {
        let ev = StreamEvent::Token("Das ".to_string());
        assert_eq!(
            ev.to_sse_frame(),
            "event: token\ndata: {\"delta\":\"Das \"}\n\n"
        );
        assert!(!ev.is_terminal());
    }

    #[test]
    fn done_frame_is_terminal() {
        let ev = StreamEvent::Done(json!({ "surface": { "components": [] } }));
        assert_eq!(ev.event_name(), "done");
        assert!(ev.is_terminal());
        assert!(ev.to_sse_frame().starts_with("event: done\ndata: "));
    }

    #[test]
    fn error_frame_carries_class_and_message() {
        let ev = StreamEvent::Error(TritonError::Tool("upstream boom".into()));
        assert!(ev.is_terminal());
        assert_eq!(
            ev.to_sse_frame(),
            "event: error\ndata: {\"error\":\"tool\",\"message\":\"tool: upstream boom\"}\n\n"
        );
    }

    #[test]
    fn parse_unknown_event_name_maps_to_tool() {
        let ev = StreamEvent::parse("retrieval", "{\"stage\":\"expand\"}");
        match ev {
            StreamEvent::Tool(v) => assert_eq!(v, json!({ "stage": "expand" })),
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn parse_token_extracts_delta() {
        match StreamEvent::parse("token", "{\"delta\":\"Kinder\"}") {
            StreamEvent::Token(d) => assert_eq!(d, "Kinder"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[test]
    fn parse_done_round_trips_value() {
        let ev = StreamEvent::Done(json!({ "surface": { "components": [42] } }));
        let frame = ev.data().to_string();
        match StreamEvent::parse("done", &frame) {
            StreamEvent::Done(v) => assert_eq!(v, json!({ "surface": { "components": [42] } })),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parse_non_json_data_falls_back_to_string() {
        match StreamEvent::parse("done", "not json") {
            StreamEvent::Done(v) => assert_eq!(v, json!("not json")),
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
