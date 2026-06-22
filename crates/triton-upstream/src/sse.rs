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

use futures::Stream;
use futures::stream::{self, BoxStream, StreamExt};
use triton_core::StreamEvent;

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
pub fn decode_sse<S, B, E>(inner: S) -> BoxStream<'static, StreamEvent>
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
    stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => {
                    st.buf.extend_from_slice(chunk.as_ref());
                    drain_frames(&mut st.buf, &mut st.pending);
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
        decode_sse(s).collect::<Vec<_>>().await
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
    async fn handles_crlf_line_endings() {
        let evs = collect(vec!["event: token\r\ndata: {\"delta\":\"x\"}\r\n\r\n"]).await;
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::Token(d) => assert_eq!(d, "x"),
            other => panic!("expected token, got {other:?}"),
        }
    }
}
