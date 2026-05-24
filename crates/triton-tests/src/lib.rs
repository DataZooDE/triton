//! Integration-test harness. The harness itself is library code; the
//! actual tests live in `tests/`. No mocks allowed — the harness
//! spawns the real `triton` binary and drives it over real HTTP.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A spawned `triton` process under test.
///
/// `Drop` kills the child so a failing test never leaks a process.
pub struct TritonProcess {
    child: Child,
    pub rest_addr: SocketAddr,
}

impl TritonProcess {
    /// Spawn the `triton` binary on a free localhost port and wait for
    /// `/healthz` to return 200. Panics if the binary doesn't come up
    /// within `boot_deadline`.
    pub async fn spawn() -> Self {
        Self::spawn_with(Duration::from_secs(5)).await
    }

    pub async fn spawn_with(boot_deadline: Duration) -> Self {
        let rest_port = free_tcp_port();
        let bin = triton_binary_path();

        let child = Command::new(&bin)
            .env("TRITON_REST_PORT", rest_port.to_string())
            .env("TRITON_HOST", "127.0.0.1")
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", bin.display()));

        let rest_addr: SocketAddr = format!("127.0.0.1:{rest_port}").parse().unwrap();
        let proc = Self { child, rest_addr };
        proc.wait_for_healthz(boot_deadline).await;
        proc
    }

    async fn wait_for_healthz(&self, deadline: Duration) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let url = format!("http://{}/healthz", self.rest_addr);
        let start = Instant::now();
        loop {
            if let Ok(resp) = client.get(&url).send().await
                && resp.status().is_success()
            {
                return;
            }
            if start.elapsed() > deadline {
                panic!("triton did not become healthy on {url} within {deadline:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn rest_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.rest_addr)
    }
}

impl Drop for TritonProcess {
    fn drop(&mut self) {
        // Best-effort: SIGKILL via std. SIGTERM-drain testing has its
        // own helper (PR 2). Here we only need teardown.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

fn triton_binary_path() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for binaries in the same workspace
    // when the test crate declares them in [[bin]] / a dev-dep. We rely
    // on cargo's standard search instead: the binary lives next to this
    // test crate's target dir under `target/<profile>/triton`.
    //
    // The integration-test crate depends on `triton-bin` as a `bin`
    // dependency declared via the env var `CARGO_BIN_EXE_triton`,
    // which cargo populates automatically for tests in the workspace
    // that have the binary in their dependency tree.
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    // Fallback: walk up from this file to the workspace root.
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let candidate_release = here.join("target/release/triton");
        let candidate_debug = here.join("target/debug/triton");
        if candidate_release.exists() {
            return candidate_release;
        }
        if candidate_debug.exists() {
            return candidate_debug;
        }
        here.pop();
    }
    panic!("could not locate `triton` binary; run `cargo build` first");
}
