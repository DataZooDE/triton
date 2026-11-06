//! `EmbedOpts::action_key`: a host that reads the A2UI action key from its
//! own source (a key file) hands it to the embedded host. That key signs
//! and resolves card actions, and the host does not log that
//! `TRITON_A2UI_ACTION_KEY` is unset.
//!
//! The key is process-wide (installed once), so each case runs in a fresh
//! child process: this test binary re-executed on one `#[ignore]`d case,
//! with its stderr captured.

use std::process::Command;
use std::sync::Arc;

use triton_core::a2ui::ge::ActionSigner;
use triton_core::dispatcher::DispatchControls;
use triton_core::{Dispatcher, ToolRegistry};
use triton_embed::{EmbedOpts, router};

const KEY: &str = "embed-host-action-key-from-a-file-0123456789";

fn build(opts: EmbedOpts) {
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(ToolRegistry::new()),
        "test",
        DispatchControls::unenforced(),
    ));
    let _ = router(dispatcher, &opts);
}

/// Child: build the router with the supplied key, then prove the global
/// signer is that key (a signer over `KEY` opens what the global one signs,
/// and the global one opens what a signer over `KEY` signs).
#[test]
#[ignore = "child process of supplied_key_signs_and_no_unset_warning"]
fn child_supplied_key() {
    build(EmbedOpts::dev().action_key(KEY));
    let ours = ActionSigner::new(triton_correlation::KeyRing::single(KEY).unwrap());
    let id = format!("a0-{}", ActionSigner::global().sign("hello").expect("sign"));
    assert_eq!(
        ours.open(&id).as_deref(),
        Some("hello"),
        "global signer is not the supplied key"
    );
    let back = format!("a0-{}", ours.sign("back").expect("sign"));
    assert_eq!(ActionSigner::global().open(&back).as_deref(), Some("back"));
}

/// Child: no key anywhere — the warning the parent must be able to see.
#[test]
#[ignore = "child process of no_key_still_warns"]
fn child_no_key() {
    build(EmbedOpts::dev());
}

fn run_child(name: &str) -> (bool, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            name,
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove("TRITON_A2UI_ACTION_KEY")
        .output()
        .expect("spawn child");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success() && all.contains("1 passed"), all)
}

#[test]
fn supplied_key_signs_and_no_unset_warning() {
    let (ok, logs) = run_child("child_supplied_key");
    assert!(ok, "child failed:\n{logs}");
    assert!(
        !logs.contains("TRITON_A2UI_ACTION_KEY unset"),
        "misleading 'unset' line with a supplied key:\n{logs}"
    );
}

#[test]
fn no_key_still_warns() {
    let (ok, logs) = run_child("child_no_key");
    assert!(ok, "child failed:\n{logs}");
    assert!(logs.contains("TRITON_A2UI_ACTION_KEY unset"), "{logs}");
}

/// Child: a malformed supplied key is `router_parts`' error, not a panic,
/// so a host reports it like any other refused setting.
#[test]
#[ignore = "child process of malformed_key_is_an_error"]
fn child_malformed_key() {
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::new(ToolRegistry::new()),
        "test",
        DispatchControls::unenforced(),
    ));
    let err = match triton_embed::router_parts(dispatcher, &EmbedOpts::dev().action_key("short")) {
        Ok(_) => panic!("a malformed action key was accepted"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("A2UI action key"), "{err}");
}

#[test]
fn malformed_key_is_an_error() {
    let (ok, logs) = run_child("child_malformed_key");
    assert!(ok, "child failed:\n{logs}");
}
