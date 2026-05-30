//! Dev-only per-`trace_id` body capture for the Explorer's Trace view
//! (issue #75). Feature-gated behind `capture`: with the feature off
//! (the default, and always in release) [`captured`] returns empty and
//! nothing is recorded — no request/response bodies are ever held in
//! memory. With the feature on, the HTTP `capture` layer in
//! `triton-adapters-http` records token-stripped request/response JSON
//! here (FR-AU-3: never the bearer), bounded to the most recent entries.

use serde_json::Value;

/// Captured bodies for `trace_id`, oldest first. Always empty unless the
/// `capture` feature is compiled in.
pub fn captured(trace_id: &str) -> Vec<Value> {
    #[cfg(feature = "capture")]
    {
        imp::captured(trace_id)
            .into_iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect()
    }
    #[cfg(not(feature = "capture"))]
    {
        let _ = trace_id;
        Vec::new()
    }
}

#[cfg(feature = "capture")]
pub use imp::record;

#[cfg(feature = "capture")]
mod imp {
    use std::collections::VecDeque;
    use std::sync::{Mutex, OnceLock};

    use serde_json::Value;

    /// Most-recent captured bodies kept in memory (dev only, no on-disk
    /// state — G-8).
    pub const CAPTURE_CAPACITY: usize = 512;

    /// One captured payload at an HTTP edge.
    #[derive(Clone, serde::Serialize)]
    pub struct CapturedBody {
        pub trace_id: String,
        /// `rest` | `mcp` | `a2a`.
        pub protocol: String,
        /// `request` | `response`.
        pub direction: String,
        pub when: String,
        pub body: Value,
    }

    struct Buf {
        inner: Mutex<VecDeque<CapturedBody>>,
    }

    fn global() -> &'static Buf {
        static B: OnceLock<Buf> = OnceLock::new();
        B.get_or_init(|| Buf {
            inner: Mutex::new(VecDeque::new()),
        })
    }

    /// Record one body (stamps the time). `body` must already be free of
    /// any bearer — we capture payloads only, never the Authorization
    /// header (FR-AU-3).
    pub fn record(trace_id: &str, protocol: &str, direction: &str, body: Value) {
        let c = CapturedBody {
            trace_id: trace_id.to_string(),
            protocol: protocol.to_string(),
            direction: direction.to_string(),
            when: chrono::Utc::now().to_rfc3339(),
            body,
        };
        let buf = global();
        let mut q = buf.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= CAPTURE_CAPACITY {
            q.pop_front();
        }
        q.push_back(c);
    }

    pub fn captured(trace_id: &str) -> Vec<CapturedBody> {
        let buf = global();
        let q = buf.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.iter()
            .filter(|c| c.trace_id == trace_id)
            .cloned()
            .collect()
    }
}
