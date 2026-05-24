//! A2A adapter — `POST /message:send` over axum. Walking scope
//! (PR 2): an empty router so the listener binds and accepts TCP
//! connections. PR 6 wires `POST /message:send` plus the
//! `InMemoryTaskStore` per FR-A-7 (G-8 forbids on-disk state).

use axum::Router;

pub fn router() -> Router {
    Router::new()
}
