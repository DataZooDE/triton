//! Process-wide settings. 12-factor §III says config comes from env
//! vars; precedence is `CLI > env > compile-time defaults`
//! (NFR-O-1). PR 2 covers the listener ports and drain deadline;
//! PR 3 adds the full struct (env label, dev-token gate, etc.).

use std::net::IpAddr;
use std::time::Duration;

/// Compile-time defaults align with FR-A-1 (ports) and FR-L-2 (drain
/// deadline of 30 s).
pub struct Settings {
    pub host: IpAddr,
    pub mcp_port: u16,
    pub a2a_port: u16,
    pub rest_port: u16,
    pub drain_deadline: Duration,
}

impl Settings {
    pub fn from_env() -> Self {
        Self {
            host: env_or("TRITON_HOST", "0.0.0.0".parse().unwrap()),
            mcp_port: env_or("TRITON_MCP_PORT", 8001),
            a2a_port: env_or("TRITON_A2A_PORT", 8002),
            rest_port: env_or("TRITON_REST_PORT", 8003),
            drain_deadline: Duration::from_secs(env_or("TRITON_DRAIN_DEADLINE_SECS", 30u64)),
        }
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
