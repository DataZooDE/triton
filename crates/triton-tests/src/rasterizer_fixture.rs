//! Harness for spawning the real `triton-rasterizer` binary.
//!
//! Mirrors [`crate::TritonProcess`]: locate the binary, bind on a
//! free port via `TRITON_RASTERIZER_PORT`, poll `/healthz` until
//! the service answers. Drop kills the child. No mocks per
//! CLAUDE.md §1 — every integration test exercising the rasterizer
//! talks to the real binary over real TCP.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct RasterizerProcess {
    child: Option<Child>,
    // Held to keep the collector threads alive; the test only
    // inspects stderr on failure paths via `stderr_snapshot()`.
    #[allow(dead_code)]
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    stdout_join: Option<JoinHandle<()>>,
    stderr_join: Option<JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl RasterizerProcess {
    pub async fn spawn() -> Self {
        Self::spawn_with(Duration::from_secs(5)).await
    }

    /// Spawn, retrying the one failure `free_tcp_port` makes possible.
    ///
    /// The port is chosen by binding `:0`, reading the number and dropping the
    /// listener — so between that drop and the child's bind, another test in
    /// this binary can take it. The child then exits at once with
    /// `Address already in use`, which surfaced as
    /// `triton-rasterizer exited early` under the full suite on 2026-09-12 and
    /// passed in isolation. `TritonProcess` has retried this since
    /// `doc/realizations.md` §7; this fixture mirrors it, and like that one it
    /// retries ONLY the collision — any other early exit is a real bug and
    /// still fails fast.
    pub async fn spawn_with(deadline: Duration) -> Self {
        const ATTEMPTS: u32 = 5;
        for attempt in 0..ATTEMPTS {
            match Self::try_spawn(deadline).await {
                Ok(p) => return p,
                Err(stderr) => {
                    assert!(
                        crate::stderr_indicates_addr_in_use(&stderr),
                        "triton-rasterizer exited early (not AddrInUse); stderr:\n{}",
                        stderr.join("\n")
                    );
                    // Brief backoff lets the OS recycle ephemeral ports.
                    tokio::time::sleep(Duration::from_millis(50 * (attempt + 1) as u64)).await;
                }
            }
        }
        panic!("triton-rasterizer lost the port race {ATTEMPTS} times running");
    }

    async fn try_spawn(deadline: Duration) -> Result<Self, Vec<String>> {
        let port = free_tcp_port();
        let bin = rasterizer_binary_path();

        let mut cmd = Command::new(&bin);
        // Scrub inherited TRITON_* env so a shell-level setting can't
        // leak into the spawned process (mirrors TritonProcess).
        for (k, _) in std::env::vars() {
            if k.starts_with("TRITON_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("TRITON_RASTERIZER_HOST", "127.0.0.1")
            .env("TRITON_RASTERIZER_PORT", port.to_string())
            .env("RUST_LOG", "info");
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", bin.display()));

        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stdout_join = spawn_collector(child.stdout.take(), stdout.clone());
        let stderr_join = spawn_collector(child.stderr.take(), stderr.clone());

        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut proc = Self {
            child: Some(child),
            stdout,
            stderr,
            stdout_join: Some(stdout_join),
            stderr_join: Some(stderr_join),
            addr,
        };
        proc.wait_for_ready(deadline).await?;
        Ok(proc)
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    pub fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr.lock().unwrap().clone()
    }

    /// `Err(stderr)` when the child exited before serving — the caller
    /// decides whether that is a lost port race (retry) or a real fault.
    async fn wait_for_ready(&mut self, deadline: Duration) -> Result<(), Vec<String>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let healthz = format!("http://{}/healthz", self.addr);
        let start = Instant::now();
        loop {
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                self.child = None;
                let stderr = self.stderr.lock().unwrap().clone();
                let _ = status;
                return Err(stderr);
            }
            if client
                .get(&healthz)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                return Ok(());
            }
            if start.elapsed() > deadline {
                let stderr = self.stderr.lock().unwrap().join("\n");
                panic!(
                    "triton-rasterizer did not become ready within {deadline:?}; stderr:\n{stderr}"
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for RasterizerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(j) = self.stdout_join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.stderr_join.take() {
            let _ = j.join();
        }
    }
}

fn spawn_collector<R>(stream: Option<R>, sink: Arc<Mutex<Vec<String>>>) -> JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    let Some(stream) = stream else {
        return thread::spawn(|| {});
    };
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').to_string();
                    sink.lock().unwrap().push(trimmed);
                }
                Err(_) => return,
            }
        }
    })
}

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

/// Locate the `triton-rasterizer` binary. Prefers
/// `CARGO_BIN_EXE_triton-rasterizer` (set automatically when the
/// integration test crate is declared with `bin` dependencies),
/// then walks up to the workspace root.
fn rasterizer_binary_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton-rasterizer") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let dbg = here.join("target/debug/triton-rasterizer");
        let rel = here.join("target/release/triton-rasterizer");
        if dbg.exists() {
            return dbg;
        }
        if rel.exists() {
            return rel;
        }
        here.pop();
    }
    panic!(
        "could not locate `triton-rasterizer` binary; run `cargo build --bin triton-rasterizer` first"
    );
}
