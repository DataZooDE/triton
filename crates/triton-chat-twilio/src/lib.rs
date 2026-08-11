//! #191 — Twilio chat-channel provider (WhatsApp via Twilio's BSP, RCS).
//!
//! This crate starts with the one core primitive every planned Twilio
//! adapter (`twilio_whatsapp`, `twilio_rcs`) shares: verifying
//! `X-Twilio-Signature`. The actual `AdapterKind`s, inbound webhooks, and
//! outbound courier land in follow-up PRs once this is proven — see
//! `doc/realizations.md` for the PR sequence.

pub mod courier;
pub mod rcs;
pub mod signature;
pub mod surface_mapper;
pub mod whatsapp;
pub use rcs::TwilioRcsAdapter;
pub use whatsapp::TwilioWhatsAppAdapter;

#[cfg(test)]
mod signature_tests {
    use super::signature::verify;

    /// Twilio's own documented request-validation example (stable across
    /// their SDKs' test suites for over a decade — see "Validating
    /// Requests" in Twilio's security docs). Proves the algorithm
    /// (HMAC-SHA1 over the URL + form params sorted-by-key and
    /// concatenated as `key`+`value` with no delimiter, base64-encoded)
    /// against a signature nobody in this codebase invented.
    const AUTH_TOKEN: &str = "12345";
    const URL: &str = "https://example.com/myapp.php?foo=1&bar=2";
    const EXPECTED_SIGNATURE: &str = "L/OH5YylLD5NRKLltdqwSvS0BnU=";

    fn params() -> Vec<(&'static str, &'static str)> {
        // Deliberately NOT pre-sorted — `verify` must sort them itself.
        vec![
            ("CallSid", "CA1234567890ABCDE"),
            ("Caller", "+14158675310"),
            ("From", "+14158675310"),
            ("To", "+18005551212"),
            ("Digits", "1234"),
        ]
    }

    #[test]
    fn twilio_documented_vector_verifies() {
        assert!(
            verify(URL, &params(), AUTH_TOKEN, EXPECTED_SIGNATURE),
            "Twilio's documented test vector must verify"
        );
    }

    #[test]
    fn tampered_param_is_rejected() {
        let mut tampered = params();
        tampered[0].1 = "CAdeadbeefdeadbeef";
        assert!(
            !verify(URL, &tampered, AUTH_TOKEN, EXPECTED_SIGNATURE),
            "a changed param must invalidate the signature"
        );
    }

    #[test]
    fn wrong_auth_token_is_rejected() {
        assert!(
            !verify(URL, &params(), "wrong-token", EXPECTED_SIGNATURE),
            "the wrong Auth Token must invalidate the signature"
        );
    }

    #[test]
    fn malformed_base64_header_is_rejected() {
        assert!(
            !verify(URL, &params(), AUTH_TOKEN, "not-valid-base64!!!"),
            "a malformed header must not panic or verify"
        );
    }
}
