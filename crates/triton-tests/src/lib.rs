//! Integration-test harness. The harness itself is library code; the
//! actual tests live in `tests/`. No mocks allowed — the harness
//! spawns the real `triton` binary and drives it over real HTTP.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub mod chat_courier_fixture;
pub mod discord_gateway_fixture;
mod oidc_fixture;
pub mod rasterizer_fixture;
pub mod signald_fixture;
pub mod upstream_fixture;
pub mod whatsapp_bridge_fixture;
pub use oidc_fixture::TestIssuer;

/// Captured failure context from a spawn attempt that didn't reach
/// `/healthz` (typically because the child exited early on a port
/// race).
struct SpawnFail {
    stderr: Vec<String>,
}

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
    pub metrics_addr: Option<SocketAddr>,
    pub chat_webhook_addr: Option<SocketAddr>,
    /// Single-port mode (TRITON_SINGLE_PORT): the MCP/A2A ports are never
    /// bound (the trio rides the REST port), so readiness skips their TCP
    /// probe and checks only `/healthz`.
    single_port: bool,
}

impl TritonProcess {
    /// Spawn the `triton` binary on free localhost ports and wait
    /// for `/healthz` to return 200 on the REST listener with TCP
    /// connect succeeding on the other two.
    pub async fn spawn() -> Self {
        Self::spawn_with(Duration::from_secs(5)).await
    }

    pub async fn spawn_with(boot_deadline: Duration) -> Self {
        Self::spawn_with_env(boot_deadline, HashMap::new()).await
    }

    /// Spawn with additional `TRITON_*` env vars layered on top of
    /// the harness defaults (host, ports, drain deadline).
    pub async fn spawn_with_env(
        boot_deadline: Duration,
        extra_env: HashMap<String, String>,
    ) -> Self {
        Self::spawn_with_args(boot_deadline, extra_env, Vec::new()).await
    }

    /// Spawn with extra env vars **and** extra CLI args. Used to
    /// exercise the NFR-O-1 precedence chain (CLI > env > default).
    /// Any inherited `TRITON_*` env var from the parent shell is
    /// scrubbed first so a test running with `TRITON_IMAGE_SHA` set
    /// externally gets the same view as one without.
    ///
    /// Retries up to a few times on `AddrInUse` — under heavy test
    /// parallelism the ephemeral-port probe in `free_tcp_port` can
    /// race with another harness picking the same port. See
    /// `doc/realizations.md` §7.
    pub async fn spawn_with_args(
        boot_deadline: Duration,
        extra_env: HashMap<String, String>,
        extra_args: Vec<String>,
    ) -> Self {
        const ATTEMPTS: u32 = 5;
        let mut last_stderr: Vec<String> = Vec::new();
        for attempt in 0..ATTEMPTS {
            match Self::try_spawn(boot_deadline, &extra_env, &extra_args).await {
                Ok(p) => return p,
                Err(SpawnFail { stderr }) => {
                    // Only retry the specific port-collision failure
                    // mode that motivated this loop. Any other early
                    // exit (bad CLI flag, missing JWKS endpoint,
                    // tokio runtime panic, ...) is a real bug — fail
                    // fast so the test surfaces the root cause.
                    if !stderr_indicates_addr_in_use(&stderr) {
                        panic!(
                            "triton exited early (not AddrInUse); stderr:\n{}",
                            stderr.join("\n")
                        );
                    }
                    last_stderr = stderr;
                    // Brief backoff lets the OS recycle ephemeral
                    // ports before we probe again.
                    tokio::time::sleep(Duration::from_millis(50 * (attempt + 1) as u64)).await;
                }
            }
        }
        panic!(
            "triton AddrInUse after {ATTEMPTS} attempts; last stderr:\n{}",
            last_stderr.join("\n")
        );
    }

    async fn try_spawn(
        boot_deadline: Duration,
        extra_env: &HashMap<String, String>,
        extra_args: &[String],
    ) -> Result<Self, SpawnFail> {
        let mcp_port = free_tcp_port();
        let a2a_port = free_tcp_port();
        let rest_port = free_tcp_port();
        let metrics_port = free_tcp_port();
        let chat_webhook_port = free_tcp_port();
        let bin = triton_binary_path();

        let mut cmd = Command::new(&bin);
        for (k, _) in std::env::vars() {
            if k.starts_with("TRITON_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("TRITON_HOST", "127.0.0.1")
            .env("TRITON_MCP_PORT", mcp_port.to_string())
            .env("TRITON_A2A_PORT", a2a_port.to_string())
            .env("TRITON_REST_PORT", rest_port.to_string())
            .env("TRITON_METRICS_HOST", "127.0.0.1")
            .env("TRITON_METRICS_PORT", metrics_port.to_string())
            .env("TRITON_CHAT_WEBHOOK_HOST", "127.0.0.1")
            .env("TRITON_CHAT_WEBHOOK_PORT", chat_webhook_port.to_string())
            // Keep the drain deadline short in tests so a hang fails
            // fast instead of waiting the production default of 30 s.
            .env("TRITON_DRAIN_DEADLINE_SECS", "3")
            .env("RUST_LOG", "info");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        for arg in extra_args {
            cmd.arg(arg);
        }
        let mut child = cmd
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

        let mut proc = Self {
            child: Some(child),
            stdout,
            stderr,
            stdout_join: Some(stdout_join),
            stderr_join: Some(stderr_join),
            mcp_addr: format!("127.0.0.1:{mcp_port}").parse().unwrap(),
            a2a_addr: format!("127.0.0.1:{a2a_port}").parse().unwrap(),
            rest_addr: format!("127.0.0.1:{rest_port}").parse().unwrap(),
            metrics_addr: Some(format!("127.0.0.1:{metrics_port}").parse().unwrap()),
            chat_webhook_addr: Some(format!("127.0.0.1:{chat_webhook_port}").parse().unwrap()),
            single_port: extra_env.get("TRITON_SINGLE_PORT").is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
        };
        if proc.wait_for_ready_or_early_exit(boot_deadline).await {
            Ok(proc)
        } else {
            let stderr_lines = proc.stderr_snapshot();
            // `child` is None because wait_for_exit reaped it.
            Err(SpawnFail {
                stderr: stderr_lines,
            })
        }
    }

    /// Wait until the REST listener answers `/healthz` AND TCP
    /// connects to the MCP + A2A ports succeed. Per `architecture.md`
    /// §5.2 `/healthz` lives on REST only; the bind sequence in
    /// `main.rs` makes a successful `/healthz` imply all three
    /// listeners are accepting.
    ///
    /// Returns `true` on ready, `false` if the child exited early
    /// (e.g. bind failure under a port-collision race). Reaps the
    /// child on early-exit so the harness can retry.
    async fn wait_for_ready_or_early_exit(&mut self, deadline: Duration) -> bool {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let healthz = format!("http://{}/healthz", self.rest_addr);
        let start = Instant::now();
        loop {
            // Check first whether the child already exited — saves
            // us 5s of polling on a bind failure.
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(_)) = child.try_wait()
            {
                self.child = None;
                if let Some(j) = self.stdout_join.take() {
                    let _ = j.join();
                }
                if let Some(j) = self.stderr_join.take() {
                    let _ = j.join();
                }
                return false;
            }

            let rest_ok = client
                .get(&healthz)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            // Single-port: MCP/A2A share the REST port, so /healthz alone
            // implies all three; their dedicated ports are never bound.
            let mcp_ok = self.single_port || tcp_connect_ok(self.mcp_addr).await;
            let a2a_ok = self.single_port || tcp_connect_ok(self.a2a_addr).await;
            if rest_ok && mcp_ok && a2a_ok {
                return true;
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
            let mut reader = BufReader::new(stream);
            // Manual `read_line` loop (rather than `lines().map_while`)
            // so a transient read error doesn't silently kill the
            // collector — we keep going on errors, only exit on EOF.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return, // EOF
                    Ok(_) => {
                        let trimmed = line.trim_end_matches('\n').to_string();
                        sink.lock().unwrap().push(trimmed);
                    }
                    Err(_) => return,
                }
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

/// Pick a free ephemeral TCP port (bind-then-drop probe). `pub` so a
/// test that needs to know its chat-webhook port BEFORE spawning (e.g.
/// #191's Twilio signature, which must sign the exact externally-visible
/// URL) can pick one, override `TRITON_CHAT_WEBHOOK_PORT` via
/// `spawn_with_env`'s `extra_env` (last-write-wins over the harness's own
/// internally-picked port), and build requests against that same value
/// directly — `TritonProcess::chat_webhook_addr` in that case reports the
/// harness's original (unused) pick, not the override, so don't rely on
/// it when you've overridden the port yourself.
pub fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

/// Heuristic match for "child exited because TCP bind raced with a
/// concurrent test". The error string comes from
/// `io::Error::last_os_error()` formatted via tokio/std — on linux
/// it surfaces as `kind: AddrInUse`. Match conservatively so we
/// never retry on an unrelated failure.
fn stderr_indicates_addr_in_use(stderr: &[String]) -> bool {
    stderr
        .iter()
        .any(|line| line.contains("AddrInUse") || line.contains("Address already in use"))
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

/// Locate the `triton` binary built by cargo. Prefers `CARGO_BIN_EXE_triton`
/// when set (the canonical cargo path), then falls back to walking up to
/// the workspace root.
///
/// **Order matters.** `cargo test` rebuilds the **debug** binary; the
/// release binary is whatever was last produced by `cargo build --release`
/// (often months out of date). Prefer debug first so the harness never
/// silently runs stale code. Discovered while debugging PR 4 — see
/// `doc/realizations.md` §7.
///
/// When we fall back to the path walk (no `CARGO_BIN_EXE_triton`, i.e.
/// `cargo test -p triton-tests …`), cargo does NOT rebuild `triton-bin`,
/// so the located binary can predate the source under test. That has
/// bitten us as a "flaky" test: a binary built between two features
/// silently omits the newer one, and the assertion fails for the wrong
/// reason. [`assert_binary_fresh`] turns that silent staleness into a
/// loud, actionable panic.
fn triton_binary_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        // Set only when the test lives in the binary's own package; cargo
        // then guarantees a fresh build, so no staleness check is needed.
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let candidate_debug = here.join("target/debug/triton");
        let candidate_release = here.join("target/release/triton");
        if candidate_debug.exists() {
            ensure_fresh_binary(&candidate_debug, &here, false);
            return candidate_debug;
        }
        if candidate_release.exists() {
            ensure_fresh_binary(&candidate_release, &here, true);
            return candidate_release;
        }
        here.pop();
    }
    panic!("could not locate `triton` binary; run `cargo build` first");
}

/// Make sure the located `triton` binary reflects the code under test.
///
/// The harness discovers the binary by path (no `CARGO_BIN_EXE_triton`),
/// and `cargo test -p triton-tests …` does NOT rebuild `triton-bin` — so
/// the binary can silently predate a production source change. That has
/// shown up as a "flaky" test: a binary built between two features omits
/// the newer one and an assertion fails for the wrong reason (e.g. a
/// token missing a `groups` claim that the code clearly emits). See
/// `doc/realizations.md` §7.
///
/// Strategy:
/// 1. A cheap mtime pre-check — if the binary is already newer than every
///    build input (production `*.rs`, `Cargo.toml`, `Cargo.lock`), it's
///    fresh; return immediately. This is the common case under
///    `cargo test --workspace` (which rebuilds the binary up front), so
///    there is zero overhead there.
/// 2. Otherwise defer to cargo's own **content-hash** fingerprinting:
///    run `cargo build` for the matching profile once. A bare
///    `touch`/`git checkout` that bumps mtime without changing bytes is a
///    fast no-op (so we never false-positive); genuinely-changed code is
///    rebuilt before the harness spawns it. The build runs at most once
///    per test process.
///
/// `release` selects the profile so a stale *release* binary is rebuilt
/// with `--release` rather than silently left in place behind a debug
/// rebuild (the harness prefers debug; release is only reached when no
/// debug binary exists).
fn ensure_fresh_binary(bin: &std::path::Path, workspace_root: &std::path::Path, release: bool) {
    if let Ok(bin_mtime) = bin.metadata().and_then(|m| m.modified()) {
        let mut newest: Option<std::time::SystemTime> = None;
        newest_source_mtime(&workspace_root.join("crates"), &mut newest);
        // Manifest/lockfile changes (deps, features) also invalidate the
        // binary but aren't `*.rs`, so fold the workspace root inputs in.
        for f in ["Cargo.toml", "Cargo.lock"] {
            fold_mtime(&workspace_root.join(f), &mut newest);
        }
        if newest.is_none_or(|src| bin_mtime >= src) {
            return; // binary is at least as new as every input → fresh
        }
    }

    static BUILT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let outcome = BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut args = vec!["build", "-p", "triton-bin"];
        if release {
            args.push("--release");
        }
        let out = Command::new(cargo)
            .args(&args)
            .output()
            .map_err(|e| format!("spawning `cargo build {}`: {e}", args.join(" ")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    });
    if let Err(e) = outcome {
        panic!(
            "the `triton` binary looked stale and rebuilding it failed:\n{e}\n\
             (run `cargo build -p triton-bin` yourself; see doc/realizations.md §7)"
        );
    }
}

/// Fold a single file's mtime into `newest` (best-effort; missing or
/// unreadable files are ignored).
fn fold_mtime(path: &std::path::Path, newest: &mut Option<std::time::SystemTime>) {
    if let Ok(mtime) = path.metadata().and_then(|m| m.modified())
        && newest.is_none_or(|n| mtime > n)
    {
        *newest = Some(mtime);
    }
}

/// Recursively track the newest mtime among build inputs (`*.rs` and
/// per-crate `Cargo.toml`) under `dir`, skipping `target` (build output)
/// and the `triton-tests` crate itself (the binary doesn't depend on it,
/// so editing a test/fixture must not read as a stale binary).
/// Best-effort: unreadable entries are skipped.
fn newest_source_mtime(dir: &std::path::Path, newest: &mut Option<std::time::SystemTime>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "triton-tests")
            {
                continue;
            }
            newest_source_mtime(&path, newest);
        } else if (path.extension().is_some_and(|e| e == "rs")
            || path.file_name().is_some_and(|n| n == "Cargo.toml"))
            && let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
            && newest.is_none_or(|n| mtime > n)
        {
            *newest = Some(mtime);
        }
    }
}
