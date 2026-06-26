//! Decode an upstream agent's `text/event-stream` response body into a
//! stream of typed [`StreamEvent`]s (issue #132).
//!
//! This is the wire-parsing half of the streaming upstream contract:
//! Triton POSTs with `Accept: text/event-stream`, the agent emits SSE
//! frames (`event: <name>` + `data: <json>`, blank-line separated), and
//! we turn each complete frame back into a [`StreamEvent`] via
//! [`StreamEvent::parse`]. Unknown event names map to `Tool` upstream of
//! here, so a newer agent vocabulary is relayed rather than rejected.

use std::collections::VecDeque;
use std::time::Duration;

use futures::Stream;
use futures::stream::{self, BoxStream, StreamExt};
use triton_core::StreamEvent;

/// Cap on the unterminated (partial-frame) buffer. A well-behaved agent
/// closes every frame with a blank line well under this; an upstream that
/// streams bytes without a boundary would otherwise grow the buffer until
/// OOM. On overflow we emit a terminal error and stop — Triton is an
/// ingress gateway and must not let a single upstream exhaust its memory.
/// 1 MiB is generous for an A2UI `done` envelope (UI components, not
/// rasterised assets, which are produced server-side, not streamed).
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Internal accumulator threaded through [`futures::stream::unfold`].
struct Decoder<S> {
    inner: S,
    buf: Vec<u8>,
    pending: VecDeque<StreamEvent>,
    done: bool,
}

/// Wrap a byte stream (e.g. `reqwest::Response::bytes_stream`) and yield
/// one [`StreamEvent`] per complete SSE frame. A transport error
/// mid-stream becomes a terminal `Error` event; a clean close flushes any
/// trailing frame and ends.
///
/// `idle_timeout` bounds the wait for the *next* byte chunk (TS-03): the
/// streaming upstream client carries no total timeout (so a legitimately
/// long stream isn't killed), so a hung/silent upstream is instead cut
/// here — if no byte arrives within the window we emit a terminal `Error`
/// and stop, which the dispatcher's `Finalized` maps to a breaker arm.
///
/// `max_duration` bounds the *total* wall-clock of the stream (Codex
/// security review): the idle timeout only bounds gaps, so an upstream
/// that drips one byte just under the idle window forever would otherwise
/// pin the connection indefinitely. Once the deadline passes we emit a
/// terminal `Error` and stop, regardless of how steadily bytes arrive.
/// Both limits carry a distinct machine `status_detail`.
pub fn decode_sse<S, B, E>(
    inner: S,
    idle_timeout: Duration,
    max_duration: Duration,
) -> BoxStream<'static, StreamEvent>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]>,
    E: std::fmt::Display + Send + 'static,
{
    let state = Decoder {
        inner: Box::pin(inner),
        buf: Vec::new(),
        pending: VecDeque::new(),
        done: false,
    };
    let deadline = tokio::time::Instant::now() + max_duration;
    stream::unfold(state, move |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            // Total-duration cap: a slow-drip upstream keeps resetting the
            // idle timer, so bound the whole stream too.
            let now = tokio::time::Instant::now();
            if now >= deadline {
                st.done = true;
                st.pending.push_back(max_duration_error(max_duration));
                continue;
            }
            // Wait at most until whichever limit is sooner, then classify.
            let wait = idle_timeout.min(deadline - now);
            let next = match tokio::time::timeout(wait, st.inner.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    st.done = true;
                    let ev = if tokio::time::Instant::now() >= deadline {
                        max_duration_error(max_duration)
                    } else {
                        // No byte within the idle window → treat the upstream
                        // as hung and fail the stream closed.
                        StreamEvent::error_with_detail(
                            triton_core::TritonError::Tool(format!(
                                "upstream stream idle timeout ({}ms without a frame)",
                                idle_timeout.as_millis()
                            )),
                            "upstream_stream_idle_timeout",
                        )
                    };
                    st.pending.push_back(ev);
                    continue;
                }
            };
            match next {
                Some(Ok(chunk)) => {
                    st.buf.extend_from_slice(chunk.as_ref());
                    drain_frames(&mut st.buf, &mut st.pending);
                    // After draining complete frames, whatever remains is a
                    // single in-progress frame. If it has blown past the cap
                    // the upstream is streaming without a boundary — fail the
                    // stream rather than buffer unboundedly.
                    if st.buf.len() > MAX_FRAME_BYTES {
                        st.done = true;
                        st.buf.clear();
                        st.pending.push_back(StreamEvent::error_with_detail(
                            triton_core::TritonError::Tool(format!(
                                "upstream SSE frame exceeded {MAX_FRAME_BYTES} bytes without a boundary"
                            )),
                            "upstream_stream_frame_overflow",
                        ));
                    }
                }
                Some(Err(e)) => {
                    // Transport fault after the stream opened: a pass-through
                    // upstream fault (no Triton-imposed detail).
                    st.done = true;
                    st.pending
                        .push_back(StreamEvent::error(triton_core::TritonError::Tool(format!(
                            "upstream stream error: {e}"
                        ))));
                }
                None => {
                    // Clean EOF: flush a trailing frame that wasn't
                    // blank-line terminated, then end.
                    st.done = true;
                    if let Some(ev) = parse_frame(&st.buf) {
                        st.pending.push_back(ev);
                    }
                    st.buf.clear();
                }
            }
        }
    })
    .boxed()
}

/// The terminal error for a stream that ran past its total-duration cap.
fn max_duration_error(max_duration: Duration) -> StreamEvent {
    StreamEvent::error_with_detail(
        triton_core::TritonError::Tool(format!(
            "upstream stream exceeded max duration ({}ms)",
            max_duration.as_millis()
        )),
        "upstream_stream_max_duration",
    )
}

/// Pull every complete (blank-line-terminated) frame out of `buf` into
/// `out`, leaving any partial trailing frame in `buf`.
fn drain_frames(buf: &mut Vec<u8>, out: &mut VecDeque<StreamEvent>) {
    while let Some((end, sep)) = find_boundary(buf) {
        let frame: Vec<u8> = buf.drain(..end + sep).collect();
        if let Some(ev) = parse_frame(&frame[..end]) {
            out.push_back(ev);
        }
    }
}

/// Index of the first frame boundary and the separator length, handling
/// both `\n\n` and `\r\n\r\n` line endings.
fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Parse one frame body (no trailing blank line) into a [`StreamEvent`].
/// Returns `None` for a frame with no `data:` and no `event:` (e.g. a
/// keep-alive comment line), which has nothing to relay.
fn parse_frame(block: &[u8]) -> Option<StreamEvent> {
    let text = std::str::from_utf8(block).ok()?;
    let mut event: Option<&str> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with(':') {
            continue; // blank or comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""), // a bare field name with no value
        };
        match field {
            "event" => event = Some(value),
            "data" => data_lines.push(value),
            _ => {} // id, retry, … not part of the Triton vocabulary
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    // SSE default event type is "message"; that maps to Tool upstream.
    Some(StreamEvent::parse(event.unwrap_or("message"), &data))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(chunks: Vec<&'static str>) -> Vec<StreamEvent> {
        let s = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::convert::Infallible>(c.as_bytes().to_vec())),
        );
        // Generous idle + total caps — these tests exercise framing, not limits.
        decode_sse(s, Duration::from_secs(60), Duration::from_secs(3600))
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn idle_timeout_emits_terminal_error() {
        // A stream that yields one frame then hangs forever must not pin the
        // consumer: the idle timeout fires and closes the stream with an error.
        let s = stream::once(async {
            Ok::<_, std::convert::Infallible>(b"event: tool\ndata: {}\n\n".to_vec())
        })
        .chain(stream::pending());
        let evs = decode_sse(s, Duration::from_millis(50), Duration::from_secs(3600))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(evs.first(), Some(StreamEvent::Tool(_))));
        match evs.last() {
            Some(StreamEvent::Error { error, detail }) => {
                assert!(error.to_string().contains("idle timeout"), "{error}");
                assert_eq!(*detail, Some("upstream_stream_idle_timeout"));
            }
            other => panic!("expected a terminal idle-timeout error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_duration_cuts_a_slow_drip_stream() {
        // An upstream that drips a comment byte every 20ms keeps resetting
        // the idle timer forever, so only the total-duration cap can stop it
        // (Codex security review). With a 200ms idle window and a 150ms total
        // cap, the stream must terminate with a max-duration error.
        let drip = stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some((
                Ok::<_, std::convert::Infallible>(b": ping\n\n".to_vec()),
                (),
            ))
        });
        let started = tokio::time::Instant::now();
        let evs = decode_sse(drip, Duration::from_millis(200), Duration::from_millis(150))
            .collect::<Vec<_>>()
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the total-duration cap must end the drip promptly"
        );
        match evs.last() {
            Some(StreamEvent::Error { error, detail }) => {
                assert!(error.to_string().contains("max duration"), "{error}");
                assert_eq!(*detail, Some("upstream_stream_max_duration"));
            }
            other => panic!("expected a terminal max-duration error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_frames_split_across_chunk_boundaries() {
        // The `done` frame is split mid-way across two chunks.
        let evs = collect(vec![
            "event: tool\ndata: {\"step\":\"search\"}\n\n",
            "event: token\ndata: {\"delta\":\"Hi\"}\n\nevent: do",
            "ne\ndata: {\"surface\":{}}\n\n",
        ])
        .await;
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], StreamEvent::Tool(_)));
        match &evs[1] {
            StreamEvent::Token(d) => assert_eq!(d, "Hi"),
            other => panic!("expected token, got {other:?}"),
        }
        assert!(matches!(evs[2], StreamEvent::Done(_)));
    }

    #[tokio::test]
    async fn skips_comment_keepalive_frames() {
        let evs = collect(vec![": keep-alive\n\n", "event: done\ndata: {}\n\n"]).await;
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Done(_)));
    }

    #[tokio::test]
    async fn flushes_trailing_unterminated_frame_on_eof() {
        let evs = collect(vec!["event: done\ndata: {\"ok\":true}"]).await;
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Done(_)));
    }

    #[tokio::test]
    async fn caps_an_unbounded_frame_with_a_terminal_error() {
        // An upstream that streams bytes without ever closing a frame must
        // not grow the buffer without limit: we cap it and emit an error.
        let huge = "x".repeat(MAX_FRAME_BYTES + 1024);
        let evs = collect(vec![Box::leak(
            format!("event: tool\ndata: {huge}").into_boxed_str(),
        )])
        .await;
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::Error { error, detail } => {
                assert!(error.to_string().contains("exceeded"), "got {error}");
                assert_eq!(*detail, Some("upstream_stream_frame_overflow"));
            }
            other => panic!("expected a terminal error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handles_crlf_line_endings() {
        let evs = collect(vec!["event: token\r\ndata: {\"delta\":\"x\"}\r\n\r\n"]).await;
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::Token(d) => assert_eq!(d, "x"),
            other => panic!("expected token, got {other:?}"),
        }
    }
}
