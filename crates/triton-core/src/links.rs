//! Which URLs Triton turns into a one-tap link (an `open_url` action, a
//! source link, an embed). Only `https://` by default (F6): the producer is
//! LLM-driven, so a prompt-injected `javascript:`/`file:`/`http:` link must
//! never become an action on any channel.
//!
//! Local end-to-end tests run their apps on `http://127.0.0.1`, which would
//! drop every link they want to assert. [`allow_http_hosts`] admits plain
//! `http://` for an explicit host list. It is process-wide and set once at
//! boot; the binary sets it only under `TRITON_ENV=local`
//! (`TRITON_ALLOW_HTTP_LINK_HOSTS`) and refuses to start otherwise.

use std::sync::OnceLock;

static HTTP_HOSTS: OnceLock<Vec<String>> = OnceLock::new();

/// Admit `http://` links whose host is one of `hosts` (case-insensitive,
/// exact match, any port). Set once per process; a second call is ignored
/// and returns `false`.
pub fn allow_http_hosts(hosts: impl IntoIterator<Item = String>) -> bool {
    let hosts: Vec<String> = hosts
        .into_iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    HTTP_HOSTS.set(hosts).is_ok()
}

/// The hosts admitted for `http://` links (empty unless allowed).
pub fn http_hosts() -> &'static [String] {
    HTTP_HOSTS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Whether `url` may become a link: `https://`, or `http://` to an
/// admitted host.
pub fn is_allowed_link(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    http_link_allowed(url, http_hosts())
}

fn http_link_allowed(url: &str, hosts: &[String]) -> bool {
    if hosts.is_empty() || !url.starts_with("http://") {
        return false;
    }
    // Parse rather than prefix-match, so `http://127.0.0.1@evil.example/`
    // or `http://127.0.0.1.evil.example/` cannot pass as the listed host.
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["127.0.0.1".into(), "localhost".into()]
    }

    #[test]
    fn https_is_always_a_link_and_other_schemes_never() {
        assert!(is_allowed_link("https://app.example.com/a"));
        assert!(!is_allowed_link("javascript:alert(1)"));
        assert!(!is_allowed_link("file:///etc/passwd"));
    }

    #[test]
    fn http_needs_a_listed_host() {
        assert!(!http_link_allowed("http://127.0.0.1:9/a", &[]));
        assert!(http_link_allowed("http://127.0.0.1:9/a", &hosts()));
        assert!(http_link_allowed("http://LOCALHOST/a", &hosts()));
        assert!(!http_link_allowed("http://elsewhere.example/a", &hosts()));
    }

    #[test]
    fn a_lookalike_host_is_not_the_listed_one() {
        assert!(!http_link_allowed(
            "http://127.0.0.1@evil.example/",
            &hosts()
        ));
        assert!(!http_link_allowed(
            "http://127.0.0.1.evil.example/",
            &hosts()
        ));
        assert!(!http_link_allowed("http://user@127.0.0.1/", &hosts()));
        assert!(!http_link_allowed("http:/127.0.0.1/", &hosts()));
    }
}
