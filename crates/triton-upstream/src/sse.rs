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
pub fn decode_sse<S, B, E>(inner: S, idle_timeout: Duration) -> BoxStream<'static, StreamEvent>
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
    stream::unfold(state, move |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            let next = match tokio::time::timeout(idle_timeout, st.inner.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    // No byte within the idle window → treat the upstream as
                    // hung and fail the stream closed.
                    st.done = true;
                    st.pending.push_back(StreamEvent::Error(
                        triton_core::TritonError::Tool(format!(
                            "upstream stream idle timeout ({}ms without a frame)",
                            idle_timeout.as_millis()
                        )),
                    ));
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
                        st.pending.push_back(StreamEvent::Error(
                            triton_core::TritonError::Tool(format!(
                                "upstream SSE frame exceeded {MAX_FRAME_BYTES} bytes without a boundary"
                            )),
                        ));
                    }
                }
                Some(Err(e)) => {
                    // Transport fault after the stream opened: surface a
                    // terminal error event and stop.
                    st.done = true;
                    st.pending
                        .push_back(StreamEvent::Error(triton_core::TritonError::Tool(format!(
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
        // A generous idle timeout — these tests exercise framing, not idleness.
        decode_sse(s, Duration::from_secs(60))
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
        let evs = decode_sse(s, Duration::from_millis(50))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(evs.first(), Some(StreamEvent::Tool(_))));
        match evs.last() {
            Some(StreamEvent::Error(e)) => assert!(e.to_string().contains("idle timeout"), "{e}"),
            other => panic!("expected a terminal idle-timeout error, got {other:?}"),
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
            StreamEvent::Error(e) => {
                assert!(e.to_string().contains("exceeded"), "got {e}")
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
