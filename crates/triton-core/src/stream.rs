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

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::{Stream, StreamExt as _};
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
    ///
    /// `detail` is a stable, machine-readable reason for a
    /// *Triton-imposed* stream limit (idle timeout, max duration, frame
    /// overflow) that the dispatcher surfaces as the audit `status_detail`
    /// so operators can tell a gateway-cut stream from a pass-through
    /// upstream `error` frame (Codex security review). `None` for a
    /// pass-through upstream fault.
    Error {
        error: TritonError,
        detail: Option<&'static str>,
    },
}

impl StreamEvent {
    /// Terminal error with no machine `status_detail` — a pass-through
    /// upstream fault (an upstream `error` frame, a transport error, an
    /// A2UI-wrap failure).
    pub fn error(error: TritonError) -> Self {
        Self::Error {
            error,
            detail: None,
        }
    }

    /// Terminal error carrying a stable, machine-readable `status_detail`
    /// for a Triton-imposed stream limit.
    pub fn error_with_detail(error: TritonError, detail: &'static str) -> Self {
        Self::Error {
            error,
            detail: Some(detail),
        }
    }

    /// The machine-readable `status_detail` carried by a terminal error,
    /// if any. `None` for every non-error event and for pass-through
    /// upstream errors.
    pub fn failure_detail(&self) -> Option<&'static str> {
        match self {
            Self::Error { detail, .. } => *detail,
            _ => None,
        }
    }

    /// The SSE `event:` name for this variant.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Tool(_) => "tool",
            Self::Token(_) => "token",
            Self::Done(_) => "done",
            Self::Error { .. } => "error",
        }
    }

    /// True for the two stream-closing variants.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// The `data:` payload as a JSON value.
    pub fn data(&self) -> Value {
        match self {
            Self::Tool(v) | Self::Done(v) => v.clone(),
            Self::Token(delta) => json!({ "delta": delta }),
            Self::Error { error, .. } => {
                json!({ "error": error.class(), "message": error.to_string() })
            }
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
                // A pass-through upstream `error` frame: a Tool fault with
                // no Triton-imposed `status_detail`.
                Self::error(TritonError::Tool(msg))
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

/// How a streamed invocation ended — the input to the single-fire
/// finalizer that owns audit emission and circuit-breaker arming
/// (issue #132). The dispatcher keys its one ADR-6 audit line off this;
/// the upstream router keys its breaker update off the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// A terminal `Done` event passed through — clean success.
    Completed,
    /// A terminal `Error` event passed through — upstream failed after
    /// the stream had already opened (200 headers flushed). `detail`
    /// carries the terminal event's machine-readable `status_detail` (a
    /// Triton-imposed stream limit) when present, else `None` for a
    /// pass-through upstream fault.
    Failed { detail: Option<&'static str> },
    /// The inner stream ended with no terminal event — the upstream
    /// closed the connection early (truncation).
    Truncated,
    /// The consumer dropped the stream before any terminal event — the
    /// client disconnected mid-stream.
    Disconnected,
}

impl Termination {
    /// Did the call complete cleanly? Only [`Termination::Completed`] is
    /// a success; everything else is some flavour of failure/abort.
    pub fn is_success(self) -> bool {
        matches!(self, Termination::Completed)
    }
}

/// Wall-clock timing handed to the finalizer at termination.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Time-to-first-event, or `None` if the stream terminated before
    /// yielding anything.
    pub ttfb: Option<Duration>,
    /// Total wall-clock from when the stream was constructed to
    /// termination.
    pub total: Duration,
}

/// A stream combinator that runs a finalizer **exactly once** when its
/// inner [`StreamEvent`] stream reaches a terminal state — whether that
/// is a terminal `Done`/`Error` event, an early upstream close, or the
/// consumer dropping the stream (client disconnect).
///
/// This is the mechanism that preserves ADR-6 ("exactly one audit line")
/// under streaming: the outcome is only known when the stream closes, so
/// the finalizer fires there. The same combinator drives the upstream
/// router's per-tool circuit-breaker update. The finalizer is held in an
/// `Option` and consumed by whichever of {terminal item, inner-`None`,
/// `Drop`} happens first — so it can never fire twice or zero times.
///
/// Keep this the **outermost** combinator in any stack (e.g. after A2UI
/// wrapping) — otherwise a client disconnect drops an inner layer first
/// and the finalizer never observes it.
pub struct Finalized<F: FnOnce(Termination, Timing)> {
    inner: BoxStream<'static, StreamEvent>,
    finalizer: Option<F>,
    started: Instant,
    first_at: Option<Instant>,
    /// Set once a terminal `Done`/`Error` has been yielded. The stream
    /// ends immediately after — a terminal event *means* close, so we
    /// never relay events an upstream emits after its own terminal
    /// (which would arrive under an already-settled audit/breaker).
    terminated: bool,
}

impl<F: FnOnce(Termination, Timing)> Finalized<F> {
    /// Wrap `inner`, arranging `finalizer` to run once at termination.
    /// The clock starts now.
    pub fn new(inner: BoxStream<'static, StreamEvent>, finalizer: F) -> Self {
        Self {
            inner,
            finalizer: Some(finalizer),
            started: Instant::now(),
            first_at: None,
            terminated: false,
        }
    }

    fn fire(&mut self, term: Termination) {
        if self.finalizer.is_some() {
            let timing = Timing {
                ttfb: self
                    .first_at
                    .map(|t| t.saturating_duration_since(self.started)),
                total: self.started.elapsed(),
            };
            // `is_some` checked above, so the take/unwrap can't panic.
            let f = self.finalizer.take().unwrap();
            f(term, timing);
        }
    }
}

impl<F: FnOnce(Termination, Timing) + Unpin> Stream for Finalized<F> {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // A terminal event already closed the stream: end it and stop
        // polling the inner stream (so post-terminal upstream frames are
        // never relayed under a settled audit/breaker).
        if this.terminated {
            return Poll::Ready(None);
        }
        match this.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(ev)) => {
                if this.first_at.is_none() {
                    this.first_at = Some(Instant::now());
                }
                if ev.is_terminal() {
                    let term = match &ev {
                        StreamEvent::Done(_) => Termination::Completed,
                        // The only other terminal is `Error`; carry its
                        // machine-readable detail through to the audit.
                        _ => Termination::Failed {
                            detail: ev.failure_detail(),
                        },
                    };
                    this.fire(term);
                    this.terminated = true;
                }
                Poll::Ready(Some(ev))
            }
            Poll::Ready(None) => {
                this.fire(Termination::Truncated);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F: FnOnce(Termination, Timing)> Drop for Finalized<F> {
    fn drop(&mut self) {
        // Still armed → nobody reached a terminal event → the consumer
        // dropped us mid-stream. That is a client disconnect.
        self.fire(Termination::Disconnected);
    }
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
        let ev = StreamEvent::error(TritonError::Tool("upstream boom".into()));
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

    mod finalized {
        use super::super::{Finalized, StreamEvent, Termination};
        use futures::StreamExt;
        use futures::stream::{self, BoxStream};
        use serde_json::json;
        use std::sync::{Arc, Mutex};

        fn capture() -> (
            Arc<Mutex<Vec<Termination>>>,
            impl FnOnce(Termination, super::super::Timing),
        ) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let sink = seen.clone();
            (seen, move |term, _timing| sink.lock().unwrap().push(term))
        }

        fn events(evs: Vec<StreamEvent>) -> BoxStream<'static, StreamEvent> {
            stream::iter(evs).boxed()
        }

        #[tokio::test]
        async fn clean_done_fires_completed_exactly_once() {
            let (seen, fin) = capture();
            let inner = events(vec![
                StreamEvent::Tool(json!({ "step": "search" })),
                StreamEvent::Token("hi".into()),
                StreamEvent::Done(json!({ "ok": true })),
            ]);
            let mut s = Finalized::new(inner, fin);
            let mut count = 0;
            while s.next().await.is_some() {
                count += 1;
            }
            drop(s);
            assert_eq!(count, 3, "all three events should pass through");
            assert_eq!(*seen.lock().unwrap(), vec![Termination::Completed]);
        }

        #[tokio::test]
        async fn ends_after_terminal_and_drops_post_terminal_events() {
            // A misbehaving upstream emits frames AFTER its own `done`.
            // Those must NOT be relayed — the terminal closes the stream.
            let (seen, fin) = capture();
            let inner = events(vec![
                StreamEvent::Token("hi".into()),
                StreamEvent::Done(json!({ "ok": true })),
                StreamEvent::Token("leaked".into()),
                StreamEvent::Tool(json!({ "late": true })),
            ]);
            let mut s = Finalized::new(inner, fin);
            let mut kinds = Vec::new();
            while let Some(ev) = s.next().await {
                kinds.push(ev.event_name());
            }
            drop(s);
            assert_eq!(
                kinds,
                vec!["token", "done"],
                "must stop at the terminal `done`, dropping post-terminal frames"
            );
            assert_eq!(*seen.lock().unwrap(), vec![Termination::Completed]);
        }

        #[tokio::test]
        async fn terminal_error_fires_failed_once() {
            let (seen, fin) = capture();
            let inner = events(vec![
                StreamEvent::Token("partial".into()),
                StreamEvent::error(crate::error::TritonError::Tool("boom".into())),
            ]);
            let mut s = Finalized::new(inner, fin);
            while s.next().await.is_some() {}
            drop(s);
            assert_eq!(
                *seen.lock().unwrap(),
                vec![Termination::Failed { detail: None }]
            );
        }

        #[tokio::test]
        async fn terminal_error_detail_threads_to_termination() {
            // A Triton-imposed stream limit carries a machine-readable
            // detail; the finalizer must surface it so the audit can set
            // `status_detail` (Codex security review).
            let (seen, fin) = capture();
            let inner = events(vec![
                StreamEvent::Token("partial".into()),
                StreamEvent::error_with_detail(
                    crate::error::TritonError::Tool("idle".into()),
                    "upstream_stream_idle_timeout",
                ),
            ]);
            let mut s = Finalized::new(inner, fin);
            while s.next().await.is_some() {}
            drop(s);
            assert_eq!(
                *seen.lock().unwrap(),
                vec![Termination::Failed {
                    detail: Some("upstream_stream_idle_timeout")
                }]
            );
        }

        #[tokio::test]
        async fn drop_before_terminal_fires_disconnected() {
            let (seen, fin) = capture();
            let inner = events(vec![
                StreamEvent::Tool(json!({ "step": "search" })),
                StreamEvent::Token("a".into()),
                // No terminal event; consumer abandons mid-stream.
            ]);
            let mut s = Finalized::new(inner, fin);
            // Pull one item, then drop without draining to the terminal.
            let _ = s.next().await;
            drop(s);
            assert_eq!(*seen.lock().unwrap(), vec![Termination::Disconnected]);
        }

        #[tokio::test]
        async fn inner_close_without_terminal_fires_truncated() {
            let (seen, fin) = capture();
            let inner = events(vec![StreamEvent::Token("only".into())]);
            let mut s = Finalized::new(inner, fin);
            while s.next().await.is_some() {}
            drop(s);
            assert_eq!(*seen.lock().unwrap(), vec![Termination::Truncated]);
        }
    }
}
