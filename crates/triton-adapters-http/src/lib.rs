//! HTTP adapter crate. Walking-skeleton scope (PR 1): the `rest`
//! adapter exposes `/healthz` only. Subsequent PRs add `/version`,
//! `/v1/tools`, `/v1/tools/:name`, plus sibling `mcp` and `a2a`
//! routers on their own listeners.

pub mod rest;
