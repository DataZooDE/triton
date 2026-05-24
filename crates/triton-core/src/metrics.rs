//! Lightweight Prometheus exposition. Hand-rolled rather than
//! pulling in `metrics-exporter-prometheus`: our cardinality is
//! tiny (a handful of tools × four protocols × four results), the
//! exposition format is text-trivial, and architecture §11 calls
//! out the goal of a static binary (NFR-PT-2) with libc-only
//! dynamic deps.
//!
//! Tailnet-only (FR-O-3 / G-5): the binary binds a separate
//! `/metrics` listener on the Tailscale interface in `main.rs`;
//! the public REST listener never carries the route.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Cardinality cap on the `tool` label in `triton_dispatch_total`.
/// Anything past this lands under the `<overflow>` bucket. Without
/// this an unauthenticated caller can hit `/v1/tools/{unique}` and
/// grow process memory through metric labels (Codex PR 11 finding).
const MAX_DISTINCT_TOOL_LABELS: usize = 256;
const OVERFLOW_LABEL: &str = "<overflow>";

/// Process-wide metrics registry. Cheap to clone via `Arc<Metrics>`;
/// internally protected by one short-held `Mutex`.
#[derive(Default)]
pub struct Metrics {
    dispatch: Mutex<BTreeMap<DispatchKey, u64>>,
    audit: Mutex<BTreeMap<String, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DispatchKey {
    tool: String,
    protocol: String,
    result: String,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one dispatch outcome. `result` is the FR-AU-2 result
    /// string (`ok` or `error:<class>`) so the metric labels match
    /// what shows up in the audit log.
    ///
    /// Unknown `tool` labels (i.e. the caller's user-controlled
    /// path segment when the dispatcher rejects pre-registry) are
    /// bucketed under `<overflow>` once the distinct-label count
    /// exceeds [`MAX_DISTINCT_TOOL_LABELS`] — bounds memory so a
    /// burst of `/v1/tools/{random}` cannot DoS the metrics map.
    pub fn record_dispatch(&self, tool: &str, protocol: &str, result: &str) {
        let mut g = self.dispatch.lock().unwrap();
        let distinct_tools: std::collections::BTreeSet<&str> =
            g.keys().map(|k| k.tool.as_str()).collect();
        let label =
            if distinct_tools.len() >= MAX_DISTINCT_TOOL_LABELS && !distinct_tools.contains(tool) {
                OVERFLOW_LABEL.to_string()
            } else {
                tool.to_string()
            };
        let key = DispatchKey {
            tool: label,
            protocol: protocol.to_string(),
            result: result.to_string(),
        };
        *g.entry(key).or_insert(0) += 1;
    }

    /// Record one emitted audit line, broken down by phase
    /// (`dispatch` / `upstream` / `rejected`).
    pub fn record_audit(&self, phase: &str) {
        let mut g = self.audit.lock().unwrap();
        *g.entry(phase.to_string()).or_insert(0) += 1;
    }

    /// Render the current state in Prometheus text exposition.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP triton_process_up 1 while the process is serving.\n");
        out.push_str("# TYPE triton_process_up gauge\n");
        out.push_str("triton_process_up 1\n");

        out.push_str("# HELP triton_dispatch_total Dispatched tool invocations.\n");
        out.push_str("# TYPE triton_dispatch_total counter\n");
        let dispatch = self.dispatch.lock().unwrap();
        for (k, v) in dispatch.iter() {
            out.push_str(&format!(
                "triton_dispatch_total{{tool=\"{}\",protocol=\"{}\",result=\"{}\"}} {v}\n",
                escape_label(&k.tool),
                escape_label(&k.protocol),
                escape_label(&k.result),
            ));
        }
        drop(dispatch);

        out.push_str("# HELP triton_audit_lines_total Audit lines emitted to stdout.\n");
        out.push_str("# TYPE triton_audit_lines_total counter\n");
        let audit = self.audit.lock().unwrap();
        for (phase, v) in audit.iter() {
            out.push_str(&format!(
                "triton_audit_lines_total{{phase=\"{}\"}} {v}\n",
                escape_label(phase),
            ));
        }
        out
    }
}

fn escape_label(s: &str) -> String {
    // Prometheus label values: escape `\`, `"`, and newlines.
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
