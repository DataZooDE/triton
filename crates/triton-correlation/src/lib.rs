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
/// default max we'll ever emit so the same token is valid across
/// every platform that adopts callback_data semantics.
pub const PLATFORM_MAX_CALLBACK_DATA: usize = 64;

/// Discord's documented `custom_id` cap (buttons, select menus,
/// modals). PR 30 uses this for modal correlation tokens — a
/// form with several field names overflows the 64-byte Telegram
/// budget, but Discord modals comfortably fit 100.
pub const DISCORD_MAX_CUSTOM_ID: usize = 100;

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
    /// `n` is a compact keyed DIGEST of the tenant this token was minted
    /// into (#250), not the tenant string. Omitted on
    /// the unbound path so existing callers keep their wire shape and
    /// their byte budget.
    ///
    /// Without it a token is a bearer capability for `(tool, args)` and
    /// nothing else: a card rendered into tenant A's conversation is
    /// visible to everyone in that conversation and in the client's
    /// network trace, so in a deployment serving more than one tenant a
    /// sender in tenant B could replay it and get B's principal against
    /// A's arguments. The chart-image tokens already bind a tenant and
    /// an expiry; the action tokens did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<&'a str>,
    /// `x` is the expiry in unix HOURS, not seconds — four fewer digits,
    /// which matters because the whole token has to fit a platform
    /// budget as small as Telegram's 64 bytes. Hour granularity is ample
    /// for TTLs measured in days. Unbound tokens never expire,
    /// which with an 8-byte truncated HMAC makes each one a permanent
    /// oracle until the correlation key rotates.
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CompactBodyOwned {
    t: String,
    a: Value,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    x: Option<u64>,
}

/// Encode a `(tool, args)` pair into a callback-data token signed
/// with `key`. Errors when the resulting token would exceed the
/// platform's callback_data cap — the surface mapper must catch
/// that and defer the button via the usual `deferred_buttons`
/// counter so a long-args tool surfaces as a logged gap instead
/// of a Telegram 400 mid-traffic.
pub fn encode(tool: &str, args: &Value, key: &[u8]) -> Result<String, EncodeError> {
    encode_with_cap(tool, args, key, PLATFORM_MAX_CALLBACK_DATA)
}

/// Same as [`encode`] but with a caller-supplied byte cap.
/// PR 30 uses this for Discord modal correlation tokens, which
/// have a higher platform-native cap ([`DISCORD_MAX_CUSTOM_ID`])
/// than Telegram's `callback_data`. The decode side has a
/// matching `decode_with_cap` so a longer token signed for
/// Discord doesn't accidentally re-cross the 64-byte DoS gate
/// that protects Telegram-style inbounds.
pub fn encode_with_cap(
    tool: &str,
    args: &Value,
    key: &[u8],
    cap: usize,
) -> Result<String, EncodeError> {
    if tool.is_empty() {
        return Err(EncodeError::EmptyTool);
    }
    let body = CompactBody {
        t: tool,
        a: args,
        n: None,
        x: None,
    };
    finish(body, key, cap)
}

/// Serialise, sign and cap-check one body. Shared by the bound and
/// unbound mint paths so they cannot drift.
fn finish(body: CompactBody<'_>, key: &[u8], cap: usize) -> Result<String, EncodeError> {
    let body_json =
        serde_json::to_string(&body).map_err(|e| EncodeError::Serialise(e.to_string()))?;
    let mac = compute_truncated_hmac(body_json.as_bytes(), key);
    let body_b64 = URL_SAFE_NO_PAD.encode(body_json.as_bytes());
    let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
    let token = format!("{body_b64}.{mac_b64}");
    if token.len() > cap {
        return Err(EncodeError::OversizedToken {
            len: token.len(),
            cap,
        });
    }
    Ok(token)
}

/// Verify a callback token under `key` and return the recovered
/// `(tool, args)` pair. Constant-time across the HMAC compare so
/// the response timing doesn't leak whether the key match failed
/// on the first byte or the last.
///
/// Tokens longer than [`PLATFORM_MAX_CALLBACK_DATA`] are rejected
/// without ever decoding or HMACing the body. Telegram caps real
/// `callback_data` at 64 bytes, but the webhook is still an HTTP
/// boundary protected only by the inbound secret header — a
/// hostile or buggy sender could otherwise force us to allocate +
/// HMAC over a multi-megabyte body. Codex PR 21 review caught
/// this; the same rule applies to every chat-channel adapter
/// that adopts callback_data semantics.
pub fn decode(token: &str, key: &[u8]) -> Result<(String, Value), DecodeError> {
    decode_with_cap(token, key, PLATFORM_MAX_CALLBACK_DATA)
}

/// Same as [`decode`] but with a caller-supplied byte cap.
/// Discord adapter uses [`DISCORD_MAX_CUSTOM_ID`] for modal-submit
/// custom_ids; the matching `encode_with_cap` mints them.
pub fn decode_with_cap(
    token: &str,
    key: &[u8],
    cap: usize,
) -> Result<(String, Value), DecodeError> {
    if token.len() > cap {
        return Err(DecodeError::Malformed);
    }
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

/// Verify signature + shape and return the whole parsed body, so the
/// bound path can inspect the binding fields the unbound path drops.
fn decode_parsed(token: &str, key: &[u8], cap: usize) -> Result<CompactBodyOwned, DecodeError> {
    if token.len() > cap {
        return Err(DecodeError::Malformed);
    }
    let (body_b64, mac_b64) = token.split_once('.').ok_or(DecodeError::Malformed)?;
    let body = URL_SAFE_NO_PAD
        .decode(body_b64)
        .map_err(|_| DecodeError::Malformed)?;
    let presented_mac = URL_SAFE_NO_PAD
        .decode(mac_b64)
        .map_err(|_| DecodeError::Malformed)?;
    let expected = compute_truncated_hmac(&body, key);
    let lengths_match = presented_mac.len() == expected.len();
    let content_eq: bool = if lengths_match {
        presented_mac.ct_eq(&expected).into()
    } else {
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
    Ok(parsed)
}

/// Mint a token BOUND to a tenant and an expiry (#250).
///
/// The unbound [`encode_with_cap`] produces a capability for
/// `(tool, args)` and nothing else, so a card token minted into one
/// tenant's conversation is replayable by a sender in another. Binding
/// makes the token answer "who was this for, and until when" as well as
/// "what does it do".
pub fn encode_bound(
    tool: &str,
    args: &Value,
    key: &[u8],
    cap: usize,
    tenant: &str,
    ttl_secs: u64,
) -> Result<String, EncodeError> {
    if tool.is_empty() {
        return Err(EncodeError::EmptyTool);
    }
    // Hours, not seconds — see `CompactBody::x`. Rounded UP so a token
    // never expires earlier than the caller asked for.
    let exp = now_secs()
        .ok_or_else(|| EncodeError::Serialise("system clock before the epoch".into()))?
        .saturating_add(ttl_secs)
        .div_ceil(3600);
    let digest = tenant_digest(tenant, key);
    let body = CompactBody {
        t: tool,
        a: args,
        n: Some(&digest),
        x: Some(exp),
    };
    finish(body, key, cap)
}

/// Verify a token AND its binding: the tenant must equal `tenant` and
/// the expiry must not have passed.
///
/// A token carrying no binding is refused, not accepted. Accepting it
/// would leave the replay open for every card minted before this
/// shipped — and since unbound tokens never expire, "before this
/// shipped" means forever. The cost is bounded and self-healing: cards
/// already sitting in a conversation stop responding to a click and the
/// next reply mints a bound one.
pub fn decode_bound(
    token: &str,
    key: &[u8],
    cap: usize,
    tenant: &str,
) -> Result<(String, Value), DecodeError> {
    let parsed = decode_parsed(token, key, cap)?;
    let Some(bound_tenant) = parsed.n.as_deref() else {
        return Err(DecodeError::Body("token carries no tenant binding".into()));
    };
    // Not constant-time on purpose: the digest is derived from an
    // identifier both sides already know, and forging one requires
    // breaking the body HMAC that covers it.
    if bound_tenant != tenant_digest(tenant, key) {
        return Err(DecodeError::Body(
            "token was minted for another tenant".into(),
        ));
    }
    let Some(now) = now_secs() else {
        return Err(DecodeError::Body(
            "system clock before the epoch; cannot check expiry".into(),
        ));
    };
    match parsed.x {
        Some(exp_hours) if exp_hours.saturating_mul(3600) >= now => {}
        Some(_) => return Err(DecodeError::Body("token expired".into())),
        None => return Err(DecodeError::Body("token carries no expiry".into())),
    }
    Ok((parsed.t, parsed.a))
}

/// Verify only the BINDING of an already-decoded token.
///
/// Some adapters decode a callback token before they resolve the sender
/// — Google Chat routes on the token's tool to decide what to do next —
/// so the tenant is not known at decode time. They call this once the
/// principal exists, before dispatching. Same rules as
/// [`decode_bound`]: unbound and expired are both refused.
pub fn verify_tenant_binding(
    token: &str,
    key: &[u8],
    cap: usize,
    tenant: &str,
) -> Result<(), DecodeError> {
    decode_bound(token, key, cap, tenant).map(|_| ())
}

/// A compact, keyed stand-in for the tenant.
///
/// The full tenant string does not fit every platform's budget: Discord
/// caps `custom_id` at 100 characters, and a GUID tenant plus an expiry
/// pushed real buttons over it, so they were silently deferred rather
/// than rendered — binding must not cost functionality.
///
/// Six base64url characters (36 bits) is ample: this is an equality
/// check between two values both sides compute, not a secret, and
/// forging a token that names a different tenant means forging the body
/// HMAC that covers this field. Keyed so a token cannot be carried
/// between deployments that happen to share a tenant name.
fn tenant_digest(tenant: &str, key: &[u8]) -> String {
    // Domain separation from the body MAC: a body always starts with
    // `{`, so this prefix can never collide with one.
    let mut input = Vec::with_capacity(tenant.len() + 2);
    input.extend_from_slice(b"n\0");
    input.extend_from_slice(tenant.as_bytes());
    let mac = compute_truncated_hmac(&input, key);
    URL_SAFE_NO_PAD.encode(mac).chars().take(6).collect()
}

/// `None` when the clock is before the epoch (a machine mid-NTP-sync,
/// say). Callers treat that as "cannot decide" and refuse: mapping it to
/// `0` made the expiry check `exp >= 0`, i.e. every expired token passed.
fn now_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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
    fn oversized_inbound_token_rejected_without_hmac_work() {
        // Codex PR 21 review concern: decode() must reject huge
        // inbound tokens before allocating + HMACing. We can't
        // observe "no HMAC work" directly, but we can confirm the
        // outer-length reject fires by passing a 100KB blob: it
        // returns Malformed instantly (no panic, no slow path).
        let huge = "A".repeat(100_000);
        assert!(matches!(decode(&huge, KEY), Err(DecodeError::Malformed)));
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

#[cfg(test)]
mod bound_tests {
    use super::*;
    use serde_json::json;

    const KEY: &[u8] = b"k";
    const CAP: usize = 4096;

    #[test]
    fn a_bound_token_round_trips_for_its_own_tenant() {
        let t = encode_bound("narrate", &json!({"s": "a"}), KEY, CAP, "acme", 3600).unwrap();
        let (tool, args) = decode_bound(&t, KEY, CAP, "acme").unwrap();
        assert_eq!(tool, "narrate");
        assert_eq!(args["s"], "a");
    }

    #[test]
    fn another_tenant_cannot_use_it() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "acme", 3600).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "globex").is_err());
    }

    #[test]
    fn an_unbound_legacy_token_is_refused() {
        // The compatibility decision, pinned: accepting these would keep
        // the replay open for every card minted before the binding
        // shipped, and unbound tokens never expire.
        let legacy = encode_with_cap("narrate", &json!({}), KEY, CAP).unwrap();
        assert!(decode_bound(&legacy, KEY, CAP, "acme").is_err());
        // ...but it still decodes on the unbound path, so nothing else breaks.
        assert!(decode_with_cap(&legacy, KEY, CAP).is_ok());
    }

    #[test]
    fn an_expired_token_is_refused() {
        // Built directly with a past expiry: with HOUR granularity a
        // `ttl_secs = 0` token still runs to the end of the current
        // hour, so a sleep cannot make one lapse inside a test.
        let args = json!({});
        let digest = tenant_digest("acme", KEY);
        let past = CompactBody {
            t: "narrate",
            a: &args,
            n: Some(&digest),
            x: Some(now_secs().unwrap() / 3600 - 1),
        };
        let t = finish(past, KEY, CAP).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "acme").is_err());
    }

    #[test]
    fn a_ttl_rounds_up_so_a_token_never_dies_early() {
        // Hour granularity must never shorten the caller's TTL.
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "acme", 1).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "acme").is_ok());
    }

    #[test]
    fn a_realistic_bound_token_fits_discords_budget() {
        // Discord buttons were silently DEFERRED until the keyed digest
        // and hour-expiry shrank the binding — a hardening that costs
        // functionality is not a hardening. A GUID tenant with a 7-day
        // TTL is the realistic worst case.
        let t = encode_bound(
            "narrate",
            &json!({ "subject": "alice" }),
            KEY,
            DISCORD_MAX_CUSTOM_ID,
            "28c0071d-815c-4ace-a3b5-9a28bde005fd",
            7 * 24 * 3600,
        )
        .expect("a realistic bound token fits Discord's budget");
        assert!(t.len() <= DISCORD_MAX_CUSTOM_ID, "got {} bytes", t.len());
    }

    /// Telegram's 64-byte `callback_data` CANNOT carry a binding, and
    /// this pins that rather than leaving it as a comment somebody will
    /// contradict. The smallest possible bound body — one-char tool,
    /// empty args — is 66 bytes, so `encode_bound` refuses for every
    /// input at that cap. Telegram (and WhatsApp Cloud, on the same
    /// budget) therefore still mint UNBOUND tokens; that residual is
    /// recorded in doc/realizations.md §7 and needs a wire decision, not
    /// a follow-up commit.
    #[test]
    fn telegrams_budget_cannot_carry_a_binding_at_all() {
        assert!(matches!(
            encode_bound("x", &json!({}), KEY, PLATFORM_MAX_CALLBACK_DATA, "t", 3600),
            Err(EncodeError::OversizedToken { .. })
        ));
    }

    #[test]
    fn a_bound_token_still_fails_a_wrong_key() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "acme", 3600).unwrap();
        assert!(decode_bound(&t, b"other", CAP, "acme").is_err());
    }

    #[test]
    fn the_binding_costs_bytes_and_the_cap_still_applies() {
        // The tenant + expiry make the token longer; a cap that fitted
        // the unbound form may not fit the bound one, and that must
        // surface as OversizedToken rather than a silently dropped
        // binding.
        let unbound = encode_with_cap("narrate", &json!({}), KEY, CAP).unwrap();
        let bound = encode_bound("narrate", &json!({}), KEY, CAP, "acme", 3600).unwrap();
        assert!(bound.len() > unbound.len());
        assert!(matches!(
            encode_bound("narrate", &json!({}), KEY, unbound.len(), "acme", 3600),
            Err(EncodeError::OversizedToken { .. })
        ));
    }
}
