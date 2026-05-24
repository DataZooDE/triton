//! Integration-test harness. The harness itself is library code; the
//! actual tests live in `tests/`. No mocks allowed — the harness
//! spawns the real `triton` binary and drives it over real HTTP.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Signal that can be delivered to the child by [`TritonProcess::signal`].
#[derive(Clone, Copy, Debug)]
pub enum Signal {
    Term,
    Int,
}

impl Signal {
    fn as_libc(self) -> libc::c_int {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Int => libc::SIGINT,
        }
    }
}

/// A spawned `triton` process under test.
///
/// `Drop` kills the child so a failing test never leaks a process.
pub struct TritonProcess {
    child: Option<Child>,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    stdout_join: Option<JoinHandle<()>>,
    stderr_join: Option<JoinHandle<()>>,
    pub mcp_addr: SocketAddr,
    pub a2a_addr: SocketAddr,
    pub rest_addr: SocketAddr,
}

impl TritonProcess {
    /// Spawn the `triton` binary on free localhost ports and wait
    /// for `/healthz` to return 200 on the REST listener with TCP
    /// connect succeeding on the other two.
    pub async fn spawn() -> Self {
        Self::spawn_with(Duration::from_secs(5)).await
    }

    pub async fn spawn_with(boot_deadline: Duration) -> Self {
        let mcp_port = free_tcp_port();
        let a2a_port = free_tcp_port();
        let rest_port = free_tcp_port();
        let bin = triton_binary_path();

        let mut child = Command::new(&bin)
            .env("TRITON_HOST", "127.0.0.1")
            .env("TRITON_MCP_PORT", mcp_port.to_string())
            .env("TRITON_A2A_PORT", a2a_port.to_string())
            .env("TRITON_REST_PORT", rest_port.to_string())
            // Keep the drain deadline short in tests so a hang fails
            // fast instead of waiting the production default of 30 s.
            .env("TRITON_DRAIN_DEADLINE_SECS", "3")
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", bin.display()));

        // Drain stdout/stderr in background threads so the OS pipe
        // buffer cannot fill and deadlock the child. Codex flagged
        // this in PR 2 review.
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stdout_join =
            spawn_line_collector("triton-stdout", child.stdout.take(), stdout.clone());
        let stderr_join =
            spawn_line_collector("triton-stderr", child.stderr.take(), stderr.clone());

        let proc = Self {
            child: Some(child),
            stdout,
            stderr,
            stdout_join: Some(stdout_join),
            stderr_join: Some(stderr_join),
            mcp_addr: format!("127.0.0.1:{mcp_port}").parse().unwrap(),
            a2a_addr: format!("127.0.0.1:{a2a_port}").parse().unwrap(),
            rest_addr: format!("127.0.0.1:{rest_port}").parse().unwrap(),
        };
        proc.wait_for_ready(boot_deadline).await;
        proc
    }

    /// Wait until the REST listener answers `/healthz` AND TCP
    /// connects to the MCP + A2A ports succeed. Per `architecture.md`
    /// §5.2 `/healthz` lives on REST only; the bind sequence in
    /// `main.rs` makes a successful `/healthz` imply all three
    /// listeners are accepting.
    async fn wait_for_ready(&self, deadline: Duration) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let healthz = format!("http://{}/healthz", self.rest_addr);
        let start = Instant::now();
        loop {
            let rest_ok = client
                .get(&healthz)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            let mcp_ok = tcp_connect_ok(self.mcp_addr).await;
            let a2a_ok = tcp_connect_ok(self.a2a_addr).await;
            if rest_ok && mcp_ok && a2a_ok {
                return;
            }
            if start.elapsed() > deadline {
                panic!(
                    "triton not ready within {deadline:?}: \
                     rest /healthz {rest_ok}, mcp tcp {mcp_ok}, a2a tcp {a2a_ok}\n\
                     stdout: {:?}\n\
                     stderr: {:?}",
                    self.stdout_snapshot(),
                    self.stderr_snapshot(),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn rest_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.rest_addr)
    }

    pub fn mcp_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.mcp_addr)
    }

    pub fn a2a_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.a2a_addr)
    }

    /// PID of the running child, or `None` if it has already been
    /// reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Snapshot of the lines captured from the child's stdout so far.
    /// Later PRs use this to parse audit lines.
    pub fn stdout_snapshot(&self) -> Vec<String> {
        self.stdout.lock().unwrap().clone()
    }

    /// Snapshot of the lines captured from the child's stderr so far.
    pub fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr.lock().unwrap().clone()
    }

    /// Send a signal to the child and wait up to `deadline` for it
    /// to exit. Panics on timeout — a hanging drain is a real test
    /// failure, not a flake to retry. `libc::kill` directly avoids
    /// pulling in `nix` for one syscall; POSIX-only (NFR-PT-1
    /// substrate target is linux/x86_64).
    pub fn signal(&mut self, sig: Signal, deadline: Duration) -> ExitStatus {
        let pid = self.pid().expect("child already reaped");
        let pid_i32 = i32::try_from(pid).expect("pid fits in i32");
        // SAFETY: pid is a live PID owned by this process; SIGTERM
        // and SIGINT never invalidate memory.
        let rc = unsafe { libc::kill(pid_i32, sig.as_libc()) };
        assert_eq!(
            rc,
            0,
            "libc::kill({pid_i32}, {sig:?}) failed: {}",
            std::io::Error::last_os_error()
        );
        self.wait_for_exit(deadline)
    }

    /// Convenience wrapper around `signal(Signal::Term, ...)`.
    pub fn terminate(&mut self, deadline: Duration) -> ExitStatus {
        self.signal(Signal::Term, deadline)
    }

    /// Wait for the child to exit, polling `try_wait` since `std`
    /// doesn't offer a timeout-aware `wait`.
    pub fn wait_for_exit(&mut self, deadline: Duration) -> ExitStatus {
        let start = Instant::now();
        let status = loop {
            let child = self.child.as_mut().expect("child already reaped");
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() > deadline {
                        panic!(
                            "triton did not exit within {deadline:?}\n\
                             stdout: {:?}\n\
                             stderr: {:?}",
                            self.stdout_snapshot(),
                            self.stderr_snapshot(),
                        );
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("wait failed: {e}"),
            }
        };
        self.child = None;
        if let Some(j) = self.stdout_join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.stderr_join.take() {
            let _ = j.join();
        }
        status
    }
}

impl Drop for TritonProcess {
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

fn spawn_line_collector<R>(
    name: &str,
    stream: Option<R>,
    sink: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    let name = name.to_string();
    let Some(stream) = stream else {
        return thread::spawn(|| {});
    };
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        })
        .expect("spawn line collector")
}

fn _assert_pipe_types() {
    // Tiny compile-time check that the bounds on `spawn_line_collector`
    // accept the actual pipe types from `Child`. Saves a confusing
    // build error later.
    fn _stdout(s: Option<ChildStdout>, m: Arc<Mutex<Vec<String>>>) {
        let _ = spawn_line_collector("x", s, m);
    }
    fn _stderr(s: Option<ChildStderr>, m: Arc<Mutex<Vec<String>>>) {
        let _ = spawn_line_collector("x", s, m);
    }
}

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

async fn tcp_connect_ok(addr: SocketAddr) -> bool {
    tokio::time::timeout(
        Duration::from_millis(200),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .is_some()
}

fn triton_binary_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
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
