//! v0.2 PR 12 — `adapter.yaml` manifest loader + closed-set
//! validator (M-MANIFEST-1, M-COVERAGE-1, M-SECRETS-1; FR-L-4..6).
//!
//! Integration approach: load real YAML files (no in-process
//! mocks), assert the valid one parses + validates, assert each
//! invalid one refuses with a structured error. Then spawn the
//! `triton` binary with `TRITON_MANIFEST_PATH=<bad>` and confirm
//! the process exits non-zero with the validation error on stderr.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use triton_manifest::{Env, Manifest, ManifestError};
use triton_tests::TritonProcess;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn valid_manifest_parses_and_passes_all_checks() {
    let m = Manifest::load(&fixture("manifest-valid.yaml")).expect("parse");
    m.validate(Env::Production)
        .expect("valid manifest passes prod checks");
    assert_eq!(m.adapters.len(), 2);
    assert!(m.adapters.contains_key("telegram"));
    assert!(m.adapters.contains_key("discord"));
}

#[test]
fn unknown_inbound_kind_refuses_at_parse() {
    let err = Manifest::load(&fixture("manifest-unknown-kind.yaml"))
        .expect_err("M-MANIFEST-1 closed-set check must reject smoke_signal");
    let msg = err.to_string();
    assert!(
        msg.contains("smoke_signal") || msg.contains("inbound") || msg.contains("kind"),
        "error should name the offending key, got: {msg}"
    );
}

#[test]
fn missing_degrade_coverage_refuses_validate() {
    let m = Manifest::load(&fixture("manifest-missing-coverage.yaml")).expect("parse");
    let err = m
        .validate(Env::Production)
        .expect_err("M-COVERAGE-1: tool surface_components must be covered by every adapter");
    assert!(
        matches!(err, ManifestError::CoverageGap { .. }),
        "expected CoverageGap, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("buttons") && msg.contains("telegram"),
        "error should name the missing rule + adapter, got: {msg}"
    );
}

#[test]
fn zero_rate_limit_refuses_validate() {
    // A zero messages_per_sec/burst makes the token bucket reject
    // every inbound — a dead adapter. Validation must catch it.
    let m = Manifest::load(&fixture("manifest-zero-rate-limit.yaml")).expect("parse");
    let err = m
        .validate(Env::Production)
        .expect_err("zero rate_limit must be rejected");
    assert!(
        matches!(err, ManifestError::ZeroRateLimit { .. }),
        "expected ZeroRateLimit, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("messages_per_sec") && msg.contains("telegram"),
        "error should name the field + adapter, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_boots_with_valid_manifest() {
    // End-to-end: spawn the real `triton` binary with
    // TRITON_MANIFEST_PATH=<valid>, confirm it serves /healthz.
    //
    // The canonical fixture uses `env://` refs for every credential
    // (production-shaped). The resolver makes those mandatory: an
    // `env://` ref pointing at an unset variable is a hard boot
    // failure. So this test injects all the fields the fixture
    // references for both the `telegram` and `discord` adapter
    // blocks. The Discord public_key MUST decode as a valid Ed25519
    // key (32 bytes hex, on-curve); we use RFC 8032 Test 1's
    // canonical example so it's traceable.
    let env = HashMap::from([
        (
            "TRITON_MANIFEST_PATH".to_string(),
            fixture("manifest-valid.yaml").display().to_string(),
        ),
        (
            "TRITON_TG_WEBHOOK_SECRET".to_string(),
            "telegram-webhook-secret".to_string(),
        ),
        (
            "TRITON_TG_BOT_TOKEN".to_string(),
            "telegram-bot-token".to_string(),
        ),
        (
            "TRITON_TG_SENDERS".to_string(),
            r#"{"42":{"sub":"alice","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_TG_CORRELATION_KEY".to_string(),
            "telegram-correlation-key".to_string(),
        ),
        (
            "TRITON_DISCORD_PUBLIC_KEY".to_string(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a".to_string(),
        ),
        (
            "TRITON_DISCORD_BOT_TOKEN".to_string(),
            "discord-bot-token".to_string(),
        ),
        (
            "TRITON_DISCORD_SENDERS".to_string(),
            r#"{"99":{"sub":"bob","scopes":["chat"],"tenant":"acme"}}"#.to_string(),
        ),
        (
            "TRITON_DISCORD_CORRELATION_KEY".to_string(),
            "discord-correlation-key".to_string(),
        ),
    ]);
    let proc = TritonProcess::spawn_with_env(Duration::from_secs(5), env).await;
    let resp = reqwest::Client::new()
        .get(proc.rest_url("/healthz"))
        .send()
        .await
        .expect("healthz");
    assert!(resp.status().is_success());
}

#[test]
fn binary_refuses_to_boot_with_malformed_manifest() {
    // We invoke the binary directly (not the harness) because
    // the harness assumes the child becomes healthy, while here we
    // expect it to exit non-zero before binding listeners.
    let bin = locate_triton_binary();
    let out = Command::new(&bin)
        .env("TRITON_HOST", "127.0.0.1")
        // Free dummy ports — the process should never bind them.
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_ENV", "nonprod")
        .env(
            "TRITON_MANIFEST_PATH",
            fixture("manifest-unknown-kind.yaml"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn triton");
    assert_eq!(
        out.status.code(),
        Some(2),
        "binary should exit 2 on a malformed manifest, got {:?}",
        out.status
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("manifest"),
        "expected manifest error in output, got:\n{combined}"
    );
}

fn locate_triton_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_triton") {
        return PathBuf::from(p);
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while here.parent().is_some() {
        let candidate_debug = here.join("target/debug/triton");
        let candidate_release = here.join("target/release/triton");
        if candidate_debug.exists() {
            return candidate_debug;
        }
        if candidate_release.exists() {
            return candidate_release;
        }
        here.pop();
    }
    panic!("could not locate `triton` binary");
}

#[test]
fn malformed_vault_ref_refused_at_parse() {
    let err = Manifest::load(&fixture("manifest-bad-vault-ref.yaml"))
        .expect_err("malformed vault refs must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("vault") && (msg.contains("#") || msg.contains("separator")),
        "error should explain the required vault://<path>#<field> shape, got: {msg}"
    );
}

#[test]
fn wellformed_vault_ref_fails_boot_closed_after_decommission() {
    // Vault was decommissioned: a structurally VALID manifest whose
    // credentials are well-formed `vault://` refs parses and passes
    // validation, but boot MUST fail closed at secret resolution rather
    // than silently start with an unresolved credential. Run under the
    // default `local` env so the only failure mode is the resolver (no
    // NFR-S-4 egress checks in the way), and confirm the message points
    // the operator at the migration.
    let manifest = fixture("manifest-vault-decommissioned.yaml")
        .display()
        .to_string();
    let out = Command::new(locate_triton_binary())
        .env("TRITON_HOST", "127.0.0.1")
        .env("TRITON_MCP_PORT", "0")
        .env("TRITON_A2A_PORT", "0")
        .env("TRITON_REST_PORT", "0")
        .env("TRITON_METRICS_PORT", "0")
        .env("TRITON_CHAT_WEBHOOK_PORT", "0")
        .env("TRITON_MANIFEST_PATH", manifest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn triton");
    assert!(
        !out.status.success(),
        "a well-formed vault:// credential MUST fail boot closed; exit: {:?}",
        out.status.code()
    );
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        logs.to_lowercase().contains("decommission"),
        "boot failure must name the Vault decommission so the operator migrates to env://; got:\n{logs}"
    );
}

#[test]
fn unsupported_version_refused() {
    let err = Manifest::load(&fixture("manifest-wrong-version.yaml"))
        .expect_err("only documented versions accepted");
    assert!(
        matches!(err, ManifestError::UnsupportedVersion { .. }),
        "expected UnsupportedVersion, got {err:?}"
    );
}

#[test]
fn missing_scheme_credential_refused() {
    let m = Manifest::load(&fixture("manifest-missing-cred.yaml")).expect("parse");
    let err = m
        .validate(Env::Production)
        .expect_err("FR-L-4: scheme-specific credential MUST be present");
    assert!(
        matches!(err, ManifestError::MissingSchemeCredential { .. }),
        "expected MissingSchemeCredential, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("secret_token") && msg.contains("secret"),
        "error should name the scheme + missing field, got: {msg}"
    );
}

#[test]
fn literal_secret_refused_in_prod_admitted_in_dev() {
    let m = Manifest::load(&fixture("manifest-literal-secret.yaml")).expect("parse");

    // Production env: refuse.
    let err = m
        .validate(Env::Production)
        .expect_err("M-SECRETS-1: prod MUST refuse literal credentials");
    assert!(
        matches!(err, ManifestError::LiteralCredentialInProd { .. }),
        "expected LiteralCredentialInProd, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("secret") || msg.contains("literally-a-secret-string"),
        "error should name the offending field, got: {msg}"
    );

    // Dev env: admit (with a warning the caller can decide to log).
    let warnings = m
        .validate(Env::Dev)
        .expect("M-SECRETS-1: dev admits literals (with warnings)");
    assert!(
        !warnings.is_empty(),
        "dev validation must surface a warning about the literal credential"
    );
    assert!(warnings.iter().any(|w| w.contains("literal credential")));
}
