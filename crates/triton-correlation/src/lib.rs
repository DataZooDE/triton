//! v0.2 PR 21 — HMAC correlation tokens for chat callbacks.
//!
//! Architecture.md §8.7 lays out the token layout:
//! `b64url(JSON({tool, args})) || "." || b64url(HMAC-SHA256(body, key))`
//!
//! Wire constraints force two implementation choices on top of the
//! spec text:
//!
//! * **Short JSON keys.** Telegram's `callback_data` is ≤64 bytes,
//!   so the body uses `{"t": <tool>, "a": <args>}`. The dispatcher
//!   only ever sees the long-form `(tool, args)` after the decoder
//!   re-expands the names; tools never see the short keys.
//! * **Truncated HMAC.** The full SHA-256 tag is 32 bytes →
//!   43 b64url chars, more than half the callback_data budget. We
//!   truncate to 8 bytes (64 bits → 11 b64url chars) so that
//!   minimal tool/args combos (`narrate` + `{"s":"alice"}` = 33-byte
//!   JSON → 44 b64url chars) fit in the 64-byte cap with a single
//!   `.` separator (44 + 1 + 11 = 56). 64-bit forgery resistance is
//!   acceptable for short-lived chat callbacks: at the manifest's
//!   per-adapter rate limit (default ≤ 50 msg/s) an attacker would
//!   need ≥ 2^32 attempts on average — over 1000 years. Same
//!   security territory as Stripe's truncated webhook signatures
//!   and Discord's truncated Ed25519 in the docs.
//!
//! The encoder refuses to emit any token longer than
//! [`PLATFORM_MAX_CALLBACK_DATA`] (64 bytes — Telegram's documented
//! cap). Surface mappers MUST handle [`EncodeError::OversizedToken`]
//! by deferring the button via the usual `tracing::warn` channel.
//!
//! Verification is constant-time over the HMAC bytes; even on
//! length-mismatched tokens we still compute the full HMAC so the
//! callback handler does not leak the configured key length via
//! response timing (mirrors PR 13's secret-token approach).
//!
//! Token replay protection is **out of scope for this PR.** A
//! single token can be replayed by a hostile platform actor until
//! we add a (timestamp, nonce) envelope. The trust boundary is
//! still firm — replaying a token cannot impersonate a new user
//! (the sender comes from the inbound update's `from.id`, not the
//! token) — but the action it triggers will fire again. Document
//! as a known v0.2 limit; revisit when the chat platforms see
//! production traffic.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Telegram's published cap on `callback_data` bytes. Used as the
/// max we'll ever emit so the same token is valid across every
/// platform that adopts callback_data semantics.
pub const PLATFORM_MAX_CALLBACK_DATA: usize = 64;

/// HMAC truncation length in bytes. 8 bytes = 64 bits. Don't
/// change this without thinking about backwards compatibility —
/// already-encoded tokens become unverifiable if either side
/// changes the length. See the crate docs for the budget math
/// that pins this at 64-bit rather than the usual 128-bit
/// auth-tag length.
const HMAC_LEN: usize = 8;

#[derive(Debug, Serialize)]
struct CompactBody<'a> {
    /// `t` is short for `tool`; see crate docs for the wire-budget
    /// rationale.
    t: &'a str,
    a: &'a Value,
}

#[derive(Debug, Deserialize)]
struct CompactBodyOwned {
    t: String,
    a: Value,
}

/// Encode a `(tool, args)` pair into a callback-data token signed
/// with `key`. Errors when the resulting token would exceed the
/// platform's callback_data cap — the surface mapper must catch
/// that and defer the button via the usual `deferred_buttons`
/// counter so a long-args tool surfaces as a logged gap instead
/// of a Telegram 400 mid-traffic.
pub fn encode(tool: &str, args: &Value, key: &[u8]) -> Result<String, EncodeError> {
    if tool.is_empty() {
        return Err(EncodeError::EmptyTool);
    }
    let body = CompactBody { t: tool, a: args };
    let body_json =
        serde_json::to_string(&body).map_err(|e| EncodeError::Serialise(e.to_string()))?;
    let mac = compute_truncated_hmac(body_json.as_bytes(), key);
    let body_b64 = URL_SAFE_NO_PAD.encode(body_json.as_bytes());
    let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
    let token = format!("{body_b64}.{mac_b64}");
    if token.len() > PLATFORM_MAX_CALLBACK_DATA {
        return Err(EncodeError::OversizedToken {
            len: token.len(),
            cap: PLATFORM_MAX_CALLBACK_DATA,
        });
    }
    Ok(token)
}

/// Verify a callback token under `key` and return the recovered
/// `(tool, args)` pair. Constant-time across the HMAC compare so
/// the response timing doesn't leak whether the key match failed
/// on the first byte or the last.
pub fn decode(token: &str, key: &[u8]) -> Result<(String, Value), DecodeError> {
    let (body_b64, mac_b64) = token.split_once('.').ok_or(DecodeError::Malformed)?;
    let body = URL_SAFE_NO_PAD
        .decode(body_b64)
        .map_err(|_| DecodeError::Malformed)?;
    let presented_mac = URL_SAFE_NO_PAD
        .decode(mac_b64)
        .map_err(|_| DecodeError::Malformed)?;
    let expected = compute_truncated_hmac(&body, key);
    // Always run the full ct_eq even when lengths differ, so the
    // path taken doesn't depend on what's wrong with the token.
    let lengths_match = presented_mac.len() == expected.len();
    let content_eq: bool = if lengths_match {
        presented_mac.ct_eq(&expected).into()
    } else {
        // Compare presented (truncated/padded to expected length)
        // against expected — keeps the work done constant.
        let mut padded = [0u8; HMAC_LEN];
        let n = presented_mac.len().min(HMAC_LEN);
        padded[..n].copy_from_slice(&presented_mac[..n]);
        padded.ct_eq(&expected).into()
    };
    if !(content_eq && lengths_match) {
        return Err(DecodeError::BadSignature);
    }
    let parsed: CompactBodyOwned =
        serde_json::from_slice(&body).map_err(|e| DecodeError::Body(e.to_string()))?;
    if parsed.t.is_empty() {
        return Err(DecodeError::Body("empty tool".into()));
    }
    Ok((parsed.t, parsed.a))
}

fn compute_truncated_hmac(body: &[u8], key: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(body);
    let full = mac.finalize().into_bytes();
    full[..HMAC_LEN].to_vec()
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("tool name must not be empty")]
    EmptyTool,
    #[error("token would be {len} bytes; platform cap is {cap}")]
    OversizedToken { len: usize, cap: usize },
    #[error("serialise: {0}")]
    Serialise(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Token doesn't split cleanly on `.` or one of the halves
    /// isn't valid base64url. Distinct from `BadSignature` so
    /// audit lines can distinguish "platform sent garbage" from
    /// "platform forwarded a token signed under the wrong key".
    #[error("malformed token")]
    Malformed,
    #[error("HMAC signature mismatch")]
    BadSignature,
    #[error("body decode: {0}")]
    Body(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &[u8] = b"test-correlation-key-32-bytes!!!";

    #[test]
    fn round_trip_simple_tool_call() {
        let args = json!({ "s": "alice" });
        let token = encode("narrate", &args, KEY).expect("encode fits");
        assert!(token.len() <= PLATFORM_MAX_CALLBACK_DATA);
        let (tool, decoded) = decode(&token, KEY).expect("decode verifies");
        assert_eq!(tool, "narrate");
        assert_eq!(decoded, args);
    }

    #[test]
    fn wrong_key_rejects_with_bad_signature() {
        let token = encode("narrate", &json!({}), KEY).unwrap();
        let other_key = b"different-correlation-key-32!!!!";
        assert!(matches!(
            decode(&token, other_key),
            Err(DecodeError::BadSignature)
        ));
    }

    #[test]
    fn malformed_token_is_distinct_from_bad_signature() {
        // No dot at all.
        assert!(matches!(
            decode("nopayload", KEY),
            Err(DecodeError::Malformed)
        ));
        // Invalid base64.
        assert!(matches!(
            decode("!!!.!!!", KEY),
            Err(DecodeError::Malformed)
        ));
    }

    #[test]
    fn tampered_body_rejects_signature() {
        let token = encode("narrate", &json!({"s":"alice"}), KEY).unwrap();
        let (body_b64, mac_b64) = token.split_once('.').unwrap();
        let mut tampered = body_b64.to_string();
        // Flip the last char so the body decodes (different bytes)
        // but the HMAC no longer matches.
        let last = tampered.pop().unwrap();
        let flip = if last == 'A' { 'B' } else { 'A' };
        tampered.push(flip);
        let bad = format!("{tampered}.{mac_b64}");
        assert!(matches!(decode(&bad, KEY), Err(DecodeError::BadSignature)));
    }

    #[test]
    fn oversized_args_refuses_to_encode() {
        let big = "x".repeat(200);
        let err = encode("narrate", &json!({ "s": big }), KEY).expect_err("too large");
        assert!(matches!(err, EncodeError::OversizedToken { .. }));
    }

    #[test]
    fn empty_tool_name_refuses_to_encode() {
        assert!(matches!(
            encode("", &json!({}), KEY),
            Err(EncodeError::EmptyTool)
        ));
    }

    #[test]
    fn shorter_hmac_section_rejects() {
        // Forge by replacing the HMAC half with a valid-base64
        // string that decodes to fewer bytes than HMAC_LEN. The
        // length-match guard in `decode` MUST catch this even
        // though the content compare path stays constant-time.
        let token = encode("narrate", &json!({}), KEY).unwrap();
        let (body, _) = token.split_once('.').unwrap();
        // 4 base64url chars → 3 bytes, < HMAC_LEN.
        let bad = format!("{body}.AAAA");
        // Either BadSignature (length mismatch path) or Malformed
        // (b64 edge case) is acceptable; what MUST NOT happen is
        // an Ok return.
        assert!(decode(&bad, KEY).is_err());
    }

    #[test]
    fn corrupted_hmac_byte_rejects() {
        // Forge by flipping a single byte INSIDE the HMAC (decode,
        // mutate, re-encode). Modifying the b64 string directly is
        // unreliable because NO_PAD base64url silently ignores the
        // unused bits of a non-aligned last char.
        let token = encode("narrate", &json!({}), KEY).unwrap();
        let (body, mac) = token.split_once('.').unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(mac).unwrap();
        bytes[0] ^= 0xFF;
        let bad_mac = URL_SAFE_NO_PAD.encode(&bytes);
        let bad = format!("{body}.{bad_mac}");
        assert!(matches!(decode(&bad, KEY), Err(DecodeError::BadSignature)));
    }
}
