//! `TRITON_OPTIONAL_ADAPTERS` — the opt-in that lets a chat adapter
//! baked into a shared manifest be SKIPPED (with a warning) when its
//! declared `env://` credential secret is absent, instead of failing
//! the whole boot.
//!
//! Motivation: `deploy/triton/adapter.yaml` is baked into both the
//! internal upstream-dispatcher image (which needs only the WhatsApp
//! adapter, for the outbound courier) and the public chat-ingress
//! image (which needs WhatsApp + Telegram). The internal image has no
//! Telegram secret — and must NOT be given one, or it could hijack the
//! gateway's Telegram webhook. So it needs to skip `telegram` while
//! still booting `whatsapp`.
//!
//! Safety: the skip fires ONLY for the precise "`env://` ref is unset"
//! case ([`ResolveError::EnvNotSet`], surfaced through the adapter
//! `BuildError`'s `#[source]` chain) AND only for an adapter the
//! operator explicitly listed. Any other failure (malformed manifest,
//! a bad value, a missing-but-not-env credential) stays fatal, and an
//! adapter not in the optional set stays fatal for every failure — so
//! the default (empty set) is byte-for-byte today's "fail on any
//! adapter error" behaviour.

use triton_secrets::ResolveError;

/// Decide whether an adapter build failure may be skipped under the
/// optional-adapters opt-in.
///
/// Returns `Some(missing_var)` — the `env://` variable name to name in
/// the warning — when ALL of these hold:
///   * `adapter_name` (compared case-insensitively) is in `optional`, and
///   * walking `error`'s `source()` chain finds a
///     [`ResolveError::EnvNotSet`].
///
/// Returns `None` (⇒ fail boot exactly as today) otherwise: the adapter
/// isn't opted in, or the failure isn't a missing `env://` secret.
///
/// `optional` is expected to already be lowercased (see
/// `Settings::optional_adapters`); we lowercase `adapter_name` here so
/// the comparison is symmetric regardless of manifest casing.
pub fn skip_reason<'a>(
    adapter_name: &str,
    error: &'a (dyn std::error::Error + 'static),
    optional: &[String],
) -> Option<&'a str> {
    let name = adapter_name.to_ascii_lowercase();
    if !optional.contains(&name) {
        return None;
    }
    missing_env_var_in_chain(error)
}

/// Walk an error's `source()` chain and return the unset `env://`
/// variable name if any link is a [`ResolveError::EnvNotSet`]. Each
/// adapter crate's `BuildError::Resolve` marks its `ResolveError`
/// `#[source]`, so the concrete `BuildError` type stays opaque here —
/// we match on the shared `ResolveError` by downcast.
fn missing_env_var_in_chain<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    let mut current: Option<&'a (dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(resolve) = err.downcast_ref::<ResolveError>() {
            return resolve.missing_env_var();
        }
        current = err.source();
    }
    None
}

/// Handle a chat-adapter build failure in the boot loop: skip-with-warn
/// when the optional-adapters opt-in covers it, otherwise log the error
/// and exit the process non-zero exactly as the binary always has.
///
/// Returning normally means "skipped — continue booting the rest"; the
/// fatal path never returns (it `std::process::exit(2)`s). `label` is
/// the adapter-kind string used in the existing log lines (e.g.
/// `"telegram"`, `"whatsapp"`).
pub fn handle_build_error(
    name: &str,
    error: &(dyn std::error::Error + 'static),
    optional: &[String],
    label: &str,
) {
    match skip_reason(name, error, optional) {
        Some(var) => {
            tracing::warn!(
                adapter = %name,
                kind = %label,
                missing_env = %var,
                "{label} adapter SKIPPED: `env://{var}` is unset and `{name}` is in \
                 TRITON_OPTIONAL_ADAPTERS — continuing boot without it",
            );
        }
        None => {
            tracing::error!(adapter = %name, error = %error, "{label} adapter build failed");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::skip_reason;
    use triton_secrets::ResolveError;

    // A stand-in adapter BuildError that mirrors the real adapter
    // crates: a `Resolve` variant carrying a `ResolveError` as its
    // `source()`, plus a non-resolve failure mode. Hand-rolled (no
    // thiserror dep in triton-bin); the real adapter crates' wiring is
    // proven separately in `real_adapter_build_error_source_chain_*`.
    #[derive(Debug)]
    enum FakeBuildError {
        Resolve(&'static str, ResolveError),
        Malformed(&'static str),
    }

    impl std::fmt::Display for FakeBuildError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                FakeBuildError::Resolve(field, e) => {
                    write!(f, "could not resolve credential field `{field}`: {e}")
                }
                FakeBuildError::Malformed(m) => write!(f, "manifest is malformed: {m}"),
            }
        }
    }

    impl std::error::Error for FakeBuildError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                FakeBuildError::Resolve(_, e) => Some(e),
                FakeBuildError::Malformed(_) => None,
            }
        }
    }

    fn optional(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn skips_listed_adapter_on_missing_env_secret() {
        let err = FakeBuildError::Resolve(
            "inbound.secret",
            ResolveError::EnvNotSet {
                var: "TRITON_TELEGRAM_SECRET_TOKEN".into(),
            },
        );
        assert_eq!(
            skip_reason("telegram", &err, &optional(&["telegram"])),
            Some("TRITON_TELEGRAM_SECRET_TOKEN"),
        );
    }

    #[test]
    fn case_insensitive_adapter_match() {
        let err = FakeBuildError::Resolve(
            "inbound.secret",
            ResolveError::EnvNotSet {
                var: "TRITON_TELEGRAM_SECRET_TOKEN".into(),
            },
        );
        // Manifest names the adapter "Telegram"; the opt-in list is
        // lowercase. The match must still fire.
        assert_eq!(
            skip_reason("Telegram", &err, &optional(&["telegram"])),
            Some("TRITON_TELEGRAM_SECRET_TOKEN"),
        );
    }

    #[test]
    fn does_not_skip_when_adapter_not_listed() {
        let err = FakeBuildError::Resolve(
            "inbound.secret",
            ResolveError::EnvNotSet {
                var: "TRITON_TELEGRAM_SECRET_TOKEN".into(),
            },
        );
        // Empty opt-in set ⇒ today's behaviour: fail.
        assert_eq!(skip_reason("telegram", &err, &optional(&[])), None);
        // A different adapter opted in ⇒ telegram still fails.
        assert_eq!(
            skip_reason("telegram", &err, &optional(&["whatsapp"])),
            None,
        );
    }

    #[test]
    fn does_not_skip_non_missing_secret_failure() {
        // Listed optional, but the failure is NOT a missing env secret.
        let err = FakeBuildError::Malformed("identity.table is not JSON");
        assert_eq!(
            skip_reason("telegram", &err, &optional(&["telegram"])),
            None,
        );
    }

    #[test]
    fn real_adapter_build_error_source_chain_is_matched() {
        // Proof the `#[source]` wiring on a REAL adapter crate's
        // `BuildError::Resolve` exposes the inner `ResolveError` to the
        // `source()`-chain walk — not just our `FakeBuildError`. Without
        // `#[source]` this downcast would miss and the adapter would
        // (wrongly) fail boot even when opted in.
        let err = triton_chat_telegram::BuildError::Resolve(
            "inbound.secret",
            ResolveError::EnvNotSet {
                var: "TRITON_TELEGRAM_SECRET_TOKEN".into(),
            },
        );
        assert_eq!(
            skip_reason("telegram", &err, &optional(&["telegram"])),
            Some("TRITON_TELEGRAM_SECRET_TOKEN"),
        );
        // ...and a real WhatsApp build error works through the same seam.
        let wa = triton_chat_whatsapp::BuildError::Resolve(
            "inbound.secret",
            ResolveError::EnvNotSet {
                var: "TRITON_WA_APP_SECRET".into(),
            },
        );
        assert_eq!(
            skip_reason("whatsapp", &wa, &optional(&["whatsapp"])),
            Some("TRITON_WA_APP_SECRET"),
        );
    }

    #[test]
    fn does_not_skip_vault_decommissioned_even_if_listed() {
        // A Vault ref is a misconfiguration, not a substrate-injection
        // gap — it must stay fatal even for a listed adapter.
        let err = FakeBuildError::Resolve(
            "inbound.secret",
            ResolveError::VaultDecommissioned {
                ref_string: "vault://kv/data/telegram#bot_token".into(),
            },
        );
        assert_eq!(
            skip_reason("telegram", &err, &optional(&["telegram"])),
            None,
        );
    }
}
