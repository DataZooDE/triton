//! Stamp the current git SHA into the binary at compile time so
//! `GET /version` can return it without shelling out at runtime
//! (FR-O-2, architecture.md §7).
//!
//! Falls back to `"dev"` if the build happens outside a git checkout
//! (e.g. `cargo install` from a tarball, CI shallow clone with no
//! history). Operators reading `/version` see `dev` and know the
//! build was not stamped, rather than getting an empty string.
//!
//! Codex review of PR 3 flagged two staleness traps in the v1
//! implementation that we have to handle here:
//!   * Watching only `.git/HEAD` misses commits that *advance* the
//!     current branch (HEAD then stays `ref: refs/heads/X`; the only
//!     file that changed is `refs/heads/X` or `packed-refs`).
//!   * Worktrees / submodules use a `.git` *file* (not directory)
//!     pointing at the real gitdir somewhere else; the raw
//!     `.git/HEAD` path doesn't exist there.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let sha = git_short_sha().unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=TRITON_BUILD_SHA={sha}");

    // Always re-run if this file changes.
    println!("cargo:rerun-if-changed=build.rs");

    if let Some(gitdir) = resolve_gitdir() {
        // HEAD always lives in the gitdir.
        emit_rerun(&gitdir.join("HEAD"));
        // packed-refs holds packed branch tips; it may not exist
        // until `git gc`/clone, but that's fine — cargo only emits
        // a warning on a missing path, it doesn't fail.
        emit_rerun(&gitdir.join("packed-refs"));
        // If HEAD is a symbolic ref, watch the resolved ref file.
        if let Some(ref_path) = head_ref_path(&gitdir) {
            emit_rerun(&ref_path);
        }
    }
}

fn emit_rerun(p: &Path) {
    println!("cargo:rerun-if-changed={}", p.display());
}

/// Real gitdir for the current checkout. Handles worktrees and
/// submodules (where `.git` is a file pointing at the real gitdir).
fn resolve_gitdir() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(&raw);
    Some(if p.is_absolute() {
        p
    } else {
        std::env::current_dir().ok()?.join(p)
    })
}

/// Resolve `HEAD` to its underlying loose-ref path when HEAD is a
/// symbolic ref (the common branch-checkout case). Returns `None`
/// when HEAD is detached (no ref to watch — the SHA in `HEAD` itself
/// changes on every commit, and we already watch `HEAD`).
fn head_ref_path(gitdir: &Path) -> Option<PathBuf> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let rest = head.trim().strip_prefix("ref: ")?;
    Some(gitdir.join(rest))
}

fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}
