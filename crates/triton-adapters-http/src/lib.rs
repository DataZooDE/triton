//! HTTP adapter crate.
//!
//! Three sibling modules, one per protocol per ADR-1. Each builds an
//! `axum::Router` and stays in the 100–200 LOC band per ADR-6 — no
//! business logic, no audit emission, no upstream calls. Walking
//! scope so far:
//!
//! * `rest` — exposes `/healthz`. PR 3 adds `/version`, PR 5 adds
//!   `/v1/tools` and `/v1/tools/:name`.
//! * `mcp`  — listener placeholder. PR 7 hand-rolls JSON-RPC over
//!   axum (`initialize`, `tools/list`, `tools/call`, `resources/read`).
//! * `a2a`  — listener placeholder. PR 6 adds `POST /message:send`
//!   backed by an in-memory task store (FR-A-7).

pub mod a2a;
pub mod mcp;
pub mod rest;
