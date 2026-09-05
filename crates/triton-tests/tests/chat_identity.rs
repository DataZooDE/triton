//! #289 — FR-I-7 identity resolution as one owned seam.
//!
//! `SenderClaims`, `IdentityMode`, the boot-time `IdentityKind` match and
//! `resolve_via_upstream` were copied into eight adapters. The cost was
//! not the duplication itself but what it did to invariants: a rule
//! added to the seam has to be added eight times, and the eighth is
//! skipped by omission rather than by decision.
//!
//! `validate_resolved` is the live example. It guards the `upstream`
//! path in three adapters and NO path in the rest — including the
//! `sender_table` path in all eight, where the same values reach the
//! same places: `PerTenantBuckets` makes `tenant` a process-lifetime
//! map key, and `static_upstream::bearer` signs it into a token.
//!
//! These tests pin the seam's contract at the boundary that can enforce
//! it for every adapter at once: BOOT. A table an operator cannot use
//! safely should refuse the deploy that carries it, not the first
//! message after it.

use std::path::PathBuf;

fn manifest_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-vault-resolver.yaml")
        .display()
        .to_string()
}

fn locate_triton_binary() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let cand = here.join("target/debug/triton");
        if cand.exists() {
            return cand;
        }
        assert!(here.pop(), "triton binary not found");
    }
}

/// Boot with the given `TRITON_TG_SENDERS` table and return everything
/// the process wrote plus its exit code.
fn boot_with_sender_table(table: &str) -> (Option<i32>, String) {
    let out = std::process::Command::new(locate_triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest_path())
        .env("TRITON_TELEGRAM_API_BASE", "http://127.0.0.1:1")
        .env("TRITON_TG_WEBHOOK_SECRET", "secret-resolved-from-vault")
        .env("TRITON_TG_BOT_TOKEN", "12345:token")
        .env(
            "TRITON_TG_CORRELATION_KEY",
            "32byte-correlation-key-for-test!",
        )
        .env("TRITON_TG_SENDERS", table)
        // Nothing to serve; the adapter either wires or refuses, and
        // either way we want the process to finish on its own.
        .env("TRITON_DRAIN_DEADLINE_SECS", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn triton");
    let pid = out.id();
    // A successful boot runs forever; give it a moment, then stop it.
    std::thread::sleep(std::time::Duration::from_millis(700));
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let done = out.wait_with_output().expect("wait");
    (
        done.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        ),
    )
}

/// A tenant carrying whitespace is exactly what `validate_resolved`
/// exists to refuse — but that guard was only ever wired to the
/// `upstream` path. The same value from a `sender_table` reaches the
/// same places: a `PerTenantBuckets` map key and a signed upstream
/// token claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sender_table_tenant_with_whitespace_refuses_boot() {
    let (code, log) =
        boot_with_sender_table(r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"ac me"}}"#);
    assert_eq!(
        code,
        Some(2),
        "a table an operator cannot use safely must refuse the DEPLOY that \
         carries it, not the first message after it;\n{log}"
    );
    assert!(
        log.contains("tenant"),
        "the refusal must name the field to fix; got:\n{log}"
    );
}

/// The same rule for the subject, and for the length cap. One test per
/// shape, because a guard that catches only the first is a guard an
/// operator will trip on the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sender_table_subject_must_be_usable_too() {
    for (label, table) in [
        (
            "control character in sub",
            r#"{"42":{"sub":"al\nice","scopes":[],"tenant":"acme"}}"#,
        ),
        (
            "empty tenant",
            r#"{"42":{"sub":"alice","scopes":[],"tenant":""}}"#,
        ),
        (
            "empty sub",
            r#"{"42":{"sub":"","scopes":[],"tenant":"acme"}}"#,
        ),
        (
            "oversized tenant",
            // 129 bytes, one over MAX_RESOLVED_FIELD_LEN.
            r#"{"42":{"sub":"alice","scopes":[],"tenant":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
        ),
    ] {
        let (code, log) = boot_with_sender_table(table);
        assert_eq!(code, Some(2), "{label} must refuse boot;\n{log}");
    }
}

/// The other half: a well-formed table still boots. A validator that
/// refuses everything is not a validator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_well_formed_sender_table_still_boots() {
    let (_, log) = boot_with_sender_table(
        r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"},
            "43":{"sub":"29:1abc","scopes":[],"tenant":"28c0071d-815c-4ace-a3b5-9a28bde005fd"}}"#,
    );
    assert!(
        log.contains("telegram webhook adapter wired"),
        "a valid table — including a Teams-shaped id and a GUID tenant — \
         must wire; got:\n{log}"
    );
}

/// An `identity.kind` the adapter does not implement must refuse the
/// BUILD, not fall through a `match` to whichever arm happened to be
/// last. Each adapter used to carry its own hand-written guard plus an
/// unreachable `other =>` arm restating it in wording that could drift;
/// `require_supported_kind` is now the single statement of the rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identity_kind_the_adapter_does_not_implement_refuses_boot() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/manifest-telegram-unsupported-identity.yaml")
        .display()
        .to_string();
    let out = std::process::Command::new(locate_triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_ENV", "local")
        .env("TRITON_MANIFEST_PATH", manifest)
        .env("TRITON_TELEGRAM_API_BASE", "http://127.0.0.1:1")
        .env("TRITON_TG_WEBHOOK_SECRET", "secret-resolved-from-vault")
        .env("TRITON_TG_BOT_TOKEN", "12345:token")
        .env("TRITON_TG_CORRELATION_KEY", "32byte-correlation-key-for-test!")
        .env("TRITON_TG_SENDERS", "unused-under-azure")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn triton");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2), "must refuse boot;\n{log}");
    assert!(
        log.contains("identity.kind") && log.contains("Azure"),
        "the refusal must name the kind AND what the adapter does \
         support, so an operator can fix it without reading the source; \
         got:\n{log}"
    );
    assert!(
        log.contains("SenderTable") && log.contains("Upstream"),
        "…and what IS supported; got:\n{log}"
    );
}
