//! Process-wide settings. Walking-skeleton scope (PR 1): only the
//! REST host + port. PR 3 expands this into the full 12-factor
//! `Settings` struct (CLI > env > defaults precedence, all three
//! ports, env label, dev-token feature gate, etc.).

use std::net::IpAddr;

/// Minimal settings used by the walking skeleton.
pub struct Settings {
    pub host: IpAddr,
    pub rest_port: u16,
}

impl Settings {
    /// Read from `TRITON_*` env vars, falling back to compile-time
    /// defaults documented in `doc/requirements.md` §FR-A-1.
    pub fn from_env() -> Self {
        let host = std::env::var("TRITON_HOST")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "0.0.0.0".parse().unwrap());
        let rest_port = std::env::var("TRITON_REST_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8003);
        Self { host, rest_port }
    }
}
