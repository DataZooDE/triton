//! MCP adapter — hand-rolled JSON-RPC over axum (ADR-2). Walking
//! scope (PR 2): an empty router so the listener binds and accepts
//! TCP connections. PR 7 wires `initialize`, `tools/list`,
//! `tools/call`, and `resources/read` per FR-A-6.

use axum::Router;

pub fn router() -> Router {
    Router::new()
}
