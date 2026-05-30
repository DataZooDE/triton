//! HTTP adapter crate.
//!
//! Three sibling modules, one per protocol per ADR-1. Each builds an
//! `axum::Router` and stays in the 100–200 LOC band per ADR-6 — no
//! business logic, no audit emission, no upstream calls. Walking
//! scope so far:
//!
//! * `rest` — `/healthz`, `/version`, `POST /v1/tools/:name`
//!   (PR 4 added the dispatch route; PR 5 adds the `GET /v1/tools`
//!   listing and richer content-negotiation).
//! * `mcp`  — listener placeholder. PR 7 hand-rolls JSON-RPC over
//!   axum (`initialize`, `tools/list`, `tools/call`, `resources/read`).
//! * `a2a`  — listener placeholder. PR 6 adds `POST /message:send`
//!   backed by an in-memory task store (FR-A-7).

pub mod a2a;
#[cfg(feature = "capture")]
pub mod capture;
pub mod cors;
pub mod identity;
pub mod mcp;
pub mod metrics;
pub mod rest;
