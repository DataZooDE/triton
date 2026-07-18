//! `X-Twilio-Signature` verification (FR-I-8 / M-SIG-1 closed-set scheme
//! `twilio_signature`).
//!
//! Twilio's algorithm (documented in their "Security" / request-validation
//! docs, unchanged for over a decade):
//!
//! 1. Start with the full request URL, including the query string.
//! 2. For each `application/x-www-form-urlencoded` POST param, sorted by
//!    key, append `key` then `value` with no delimiter.
//! 3. `HMAC-SHA1` over that string, keyed by the account's Auth Token.
//! 4. Base64-encode the digest; compare to `X-Twilio-Signature` in
//!    constant time.
//!
//! This is distinct from `SignatureScheme::Hmac256` (which HMACs the raw
//! JSON body): Twilio signs the URL + form params, not body bytes, and
//! uses SHA-1, not SHA-256. Verify BEFORE parsing the form body (mirrors
//! every other adapter's discipline) — callers pass the raw parsed pairs
//! only after this returns `true`... in practice the caller parses the
//! `x-www-form-urlencoded` body once (cheap, no side effects) and hands
//! the pairs here before acting on any of them.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

type HmacSha1 = Hmac<Sha1>;

/// Verify `presented` (the raw `X-Twilio-Signature` header value) against
/// `url` (full request URL, including query string) + `params` (the
/// parsed form body, any order — this function sorts them) signed with
/// `auth_token`.
///
/// Returns `false` on any mismatch, including a malformed (non-base64)
/// header — never panics on attacker-controlled input.
pub fn verify(url: &str, params: &[(&str, &str)], auth_token: &str, presented: &str) -> bool {
    let Ok(presented_bytes) = STANDARD.decode(presented) else {
        return false;
    };
    let Ok(mut mac) = HmacSha1::new_from_slice(auth_token.as_bytes()) else {
        return false;
    };
    mac.update(signing_string(url, params).as_bytes());
    let computed = mac.finalize().into_bytes();
    // The length check DOES short-circuit before `ct_eq` — this only
    // ever reveals "was the decoded signature 20 bytes", never anything
    // about its content, and matches the same length-then-ct_eq shape
    // WhatsApp Cloud's own `verify_hmac256` uses (established codebase
    // precedent, not a Twilio-specific deviation; Codex review raised
    // this — see doc/realizations.md).
    presented_bytes.len() == computed.len() && presented_bytes.ct_eq(&computed).into()
}

/// Build the string Twilio signs: the URL followed by every param sorted
/// by key, each appended as `key` then `value` with no delimiter.
fn signing_string(url: &str, params: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = params.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut s = String::from(url);
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::signing_string;

    #[test]
    fn signing_string_sorts_params_by_key_regardless_of_input_order() {
        let a = signing_string("https://x", &[("b", "2"), ("a", "1")]);
        let b = signing_string("https://x", &[("a", "1"), ("b", "2")]);
        assert_eq!(a, b);
        assert_eq!(a, "https://xa1b2");
    }

    #[test]
    fn signing_string_with_no_params_is_just_the_url() {
        assert_eq!(signing_string("https://x?foo=1", &[]), "https://x?foo=1");
    }
}
