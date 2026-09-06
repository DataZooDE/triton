//! The harness must find and build the `triton` binary when it is
//! consumed from OUTSIDE this workspace.
//!
//! Triton ships two ways, and the embedded one is what runs in
//! production: a consumer vendors this repo as a submodule
//! (`vendor/triton`) and sets `exclude = ["vendor"]` in its own
//! workspace so cargo does not absorb the nested workspaces. In that
//! shape `triton-bin` is not a package of the consumer's workspace, so
//! the harness's rebuild — a bare `cargo build -p triton-bin`, inheriting
//! the consumer's cwd — resolved nothing and every spawned-binary test
//! died with:
//!
//! ```text
//! package ID specification `triton-bin` did not match any packages
//! ```
//!
//! That is 69 integration tests across `datazoo-agent-template` and
//! `heron` reporting failure for a reason unrelated to what they assert
//! — which is to say those repos ran NO no-mock integration tests at
//! all. That is exactly the blind spot that let `TRITON_DENIED_PRINCIPALS`
//! ship dead: wired in `triton-bin`'s `main`, never reached by the
//! embedded host, every test green (doc/realizations.md §9).
//!
//! The build must therefore name the workspace it belongs to, not
//! inherit whichever one the caller happens to be standing in.

/// Red before the fix: run the harness's own rebuild command from a
/// directory that is a DIFFERENT cargo workspace, the way a consumer's
/// `cargo test` does.
#[test]
fn the_rebuild_resolves_triton_bin_from_a_foreign_workspace() {
    let scratch = std::env::temp_dir().join(format!(
        "triton-consumer-cwd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(scratch.join("src")).expect("scratch dir");
    // A minimal consumer workspace that excludes its vendored copies,
    // mirroring datazoo-agent-template and heron.
    std::fs::write(
        scratch.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = []\nexclude = [\"vendor\"]\n",
    )
    .expect("write consumer manifest");

    let mut cmd = triton_tests::triton_bin_build_command(false);
    cmd.current_dir(&scratch);
    let out = cmd.output().expect("spawn cargo build");

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        out.status.success(),
        "the harness must be able to build `triton-bin` from a consumer's \
         cwd; got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The located binary must be anchored to THIS workspace, never picked
/// up by climbing into the consumer's tree above it. In-tree this passes
/// either way — the end-to-end proof is a consumer's own suite going
/// green — but it pins the anchor so a future walk-up cannot creep back.
#[test]
fn the_located_binary_lives_in_tritons_own_target_dir() {
    let root = triton_tests::triton_workspace_root();
    let bin = triton_tests::locate_triton_binary();
    assert!(
        bin.starts_with(&root),
        "located `{}`, which is outside the triton workspace at `{}`",
        bin.display(),
        root.display()
    );
    assert!(
        bin.exists(),
        "located a path that does not exist: {}",
        bin.display()
    );
}
