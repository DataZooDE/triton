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
/// Mint a bound token with an ABSOLUTE expiry (unix seconds).
///
/// [`encode_bound`] takes a relative TTL and rounds up to the next hour,
/// so no caller can produce an already-expired token — which meant the
/// expiry branch could only ever be exercised through crate internals,
/// never against a running binary (CLAUDE.md §1). This is the seam that
/// lets an integration test present a genuinely stale token to a real
/// adapter.
pub fn encode_bound_at(
    tool: &str,
    args: &Value,
    key: &[u8],
    cap: usize,
    platform: &str,
    tenant: &str,
    exp_unix_secs: u64,
) -> Result<String, EncodeError> {
    if tool.is_empty() {
        return Err(EncodeError::EmptyTool);
    }
    let body = CompactBody {
        t: tool,
        a: args,
        x: Some(exp_unix_secs.div_ceil(3600)),
    };
    finish(body, &tenant_key(key, platform, tenant), cap)
}

pub fn encode_bound(
    tool: &str,
    args: &Value,
    key: &[u8],
    cap: usize,
    platform: &str,
    tenant: &str,
    ttl_secs: Option<u64>,
) -> Result<String, EncodeError> {
    if tool.is_empty() {
        return Err(EncodeError::EmptyTool);
    }
    // Hours, not seconds — see `CompactBody::x`. Rounded UP so a token
    // never expires earlier than the caller asked for. `None` where the
    // platform's budget cannot afford the field at all (Telegram); the
    // TENANT binding is free and applies regardless.
    let exp = match ttl_secs {
        Some(ttl) => Some(
            now_secs()
                .ok_or_else(|| EncodeError::Serialise("system clock before the epoch".into()))?
                .saturating_add(ttl)
                .div_ceil(3600),
        ),
        None => None,
    };
    let body = CompactBody {
        t: tool,
        a: args,
        x: exp,
    };
    finish(body, &tenant_key(key, platform, tenant), cap)
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
    platform: &str,
    tenant: &str,
) -> Result<(String, Value), DecodeError> {
    // The tenant is in the KEY, so a token minted for another tenant
    // fails the signature check here — there is no field to compare and
    // nothing to forget to check. A pre-binding token, minted under the
    // bare key, fails the same way.
    let parsed = decode_parsed(token, &tenant_key(key, platform, tenant), cap)?;
    // An expiry is present only where the platform's token budget could
    // afford one. Its ABSENCE is not attacker-selectable: the field is
    // covered by the MAC, so stripping it invalidates the token.
    if let Some(exp_hours) = parsed.x {
        let Some(now) = now_secs() else {
            return Err(DecodeError::Body(
                "system clock before the epoch; cannot check expiry".into(),
            ));
        };
        if exp_hours.saturating_mul(3600) < now {
            return Err(DecodeError::Body("token expired".into()));
        }
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
    platform: &str,
    tenant: &str,
) -> Result<(), DecodeError> {
    decode_bound(token, key, cap, platform, tenant).map(|_| ())
}

/// Derive a per-tenant signing key — what actually binds a token to a
/// tenant, at ZERO cost on the wire.
///
/// That cost is the whole point. Carrying the tenant, even as a
/// 6-character digest, plus an expiry cost ~32 bytes; Telegram's
/// `callback_data` cap is 64 and a real unbound token already reaches
/// it, so a wire field could not be afforded there at all and Telegram
/// and WhatsApp Cloud were left with the replay open. In the key it is
/// free — and stronger, because a token minted for another tenant fails
/// the SIGNATURE rather than an equality comparison a caller could
/// forget to make.
///
/// The PLATFORM is folded in beside the tenant. Without it, two adapters
/// sharing a `correlation_key` — nothing refuses that today, and every
/// fixture uses one literal — would accept each other's tokens for the
/// same tenant and tool. That matters most for the expiry-less tokens
/// Telegram's budget forces: an immortal `callback_data` token would be
/// a byte-valid Discord `custom_id`, re-creating the forever-capability
/// this binding exists to close, one adapter over.
///
/// The label domain-separates the whole thing from the body MAC, which
/// is computed over JSON and so always begins with `{`. The original key
/// is appended so the derived key never carries less entropy than the
/// one it replaces.
fn tenant_key(key: &[u8], platform: &str, tenant: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(DERIVATION_LABEL.len() + platform.len() + tenant.len() + 2);
    input.extend_from_slice(DERIVATION_LABEL);
    input.push(0);
    input.extend_from_slice(platform.as_bytes());
    input.push(0);
    input.extend_from_slice(tenant.as_bytes());
    // The FULL tag, not the truncated one: truncation is a wire-budget
    // constraint and this key never leaves the process.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&input);
    let mut derived = mac.finalize().into_bytes().to_vec();
    derived.extend_from_slice(key);
    derived
}

/// Versioned, unambiguous prefix for the key derivation. Rotating it
/// invalidates every outstanding token, which is the point of having one.
const DERIVATION_LABEL: &[u8] = b"triton/correlation/tenant-key/v1";

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

/// The set of correlation keys a deployment will accept, newest first.
///
/// #287: before this, `correlation_key` was one value used to both sign
/// and verify, so changing it invalidated every token in flight — every
/// button already sitting in a conversation stopped responding on the
/// deploy that rotated it. Faced with "rotate and break every live
/// button", nobody rotates, and a key nobody can rotate is a key nobody
/// can recover from once it leaks.
///
/// A ring makes rotation a three-step operation with no broken window:
///
/// 1. prepend the new key — `new,old`;
/// 2. deploy: new tokens are signed with `new`, old ones still verify;
/// 3. once every token minted under `old` has expired, drop it.
///
/// The FIRST key signs. That ordering is the operator-facing contract
/// and it is what makes step 3 finite: were the last key the signer,
/// dropping the old one would change what gets minted and the window
/// would never close.
///
/// The ring holds SECRETS, so it deliberately implements neither
/// `Debug` nor `Display` — a key that reaches a log line is a key that
/// has to be rotated, and the whole point here is to make that rare.
#[derive(Clone)]
pub struct KeyRing {
    keys: Vec<Vec<u8>>,
}

impl KeyRing {
    /// Parse an operator-supplied secret: one key, or several separated
    /// by commas during a rotation. Surrounding whitespace is trimmed
    /// (a list gets pasted as `new, old` far more often than not) and
    /// empty entries are dropped, so a trailing comma is not a key.
    ///
    /// Fails when nothing survives: an empty ring would verify nothing
    /// and, worse, sign with a key that does not exist.
    pub fn parse(spec: &str) -> Result<Self, KeyRingError> {
        let keys: Vec<Vec<u8>> = spec
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(|k| k.as_bytes().to_vec())
            .collect();
        if keys.is_empty() {
            return Err(KeyRingError::Empty);
        }
        Ok(Self { keys })
    }

    /// A ring of exactly one key — the shape every caller had before
    /// rotation existed, and what tests mint under.
    pub fn single(key: impl Into<Vec<u8>>) -> Self {
        Self {
            keys: vec![key.into()],
        }
    }

    /// The key new tokens are signed with: the first on the ring.
    pub fn signing(&self) -> &[u8] {
        &self.keys[0]
    }

    /// Every key a token may have been minted under, newest first.
    pub fn verifying(&self) -> impl Iterator<Item = &[u8]> {
        self.keys.iter().map(Vec::as_slice)
    }

    /// How many keys are on the ring. Operators see this in a boot log
    /// line: a ring left at 2 forever is an unfinished rotation.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        false // `parse` and `single` both refuse to build an empty ring.
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyRingError {
    #[error("correlation key list is empty; at least one key is required")]
    Empty,
}

/// Try `verify` under each key on the ring, newest first.
///
/// Returns the first success. On failure it returns the error from the
/// FIRST key that got far enough to decode a body — an expiry failure
/// means that key's MAC passed, which is a strictly more informative
/// verdict than the `BadSignature` every other key will produce, and
/// reporting it keeps "your button timed out" distinguishable from
/// "your button was signed with a key we dropped".
fn try_ring<T>(
    ring: &KeyRing,
    verify: impl Fn(&[u8]) -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    let mut first_error: Option<DecodeError> = None;
    for key in ring.verifying() {
        match verify(key) {
            Ok(v) => return Ok(v),
            // The MAC passed under this key and the BODY was the
            // problem (expired, unparseable). No other key can do
            // better, so stop and report it.
            Err(e @ DecodeError::Body(_)) => return Err(e),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    Err(first_error.unwrap_or(DecodeError::BadSignature))
}

/// [`decode_bound`] against every key on the ring.
pub fn decode_bound_any(
    token: &str,
    ring: &KeyRing,
    cap: usize,
    platform: &str,
    tenant: &str,
) -> Result<(String, Value), DecodeError> {
    try_ring(ring, |key| decode_bound(token, key, cap, platform, tenant))
}

/// [`verify_tenant_binding`] against every key on the ring.
pub fn verify_tenant_binding_any(
    token: &str,
    ring: &KeyRing,
    cap: usize,
    platform: &str,
    tenant: &str,
) -> Result<(), DecodeError> {
    try_ring(ring, |key| {
        verify_tenant_binding(token, key, cap, platform, tenant)
    })
}

/// [`decode_with_cap`] against every key on the ring. Used by the
/// UNBOUND tokens — report images, dashboard PNGs — which carry no
/// tenant binding but rotate on the same schedule as everything else.
pub fn decode_with_cap_any(
    token: &str,
    ring: &KeyRing,
    cap: usize,
) -> Result<(String, Value), DecodeError> {
    try_ring(ring, |key| decode_with_cap(token, key, cap))
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

    const OLD: &[u8] = b"an-old-correlation-key-32-bytes!";
    const NEW: &[u8] = b"a-new-correlation-key-32-bytes!!";

    #[test]
    fn the_first_key_signs_and_every_key_verifies() {
        let ring = KeyRing::parse("new-key,old-key").expect("two keys");
        assert_eq!(ring.signing(), b"new-key");
        assert_eq!(ring.len(), 2);
        assert_eq!(
            ring.verifying().collect::<Vec<_>>(),
            vec![b"new-key".as_slice(), b"old-key".as_slice()],
        );
    }

    #[test]
    fn a_pasted_list_is_trimmed_and_a_trailing_comma_is_not_a_key() {
        let ring = KeyRing::parse(" new , old ,").expect("parses");
        assert_eq!(ring.len(), 2, "the empty tail entry is not a key");
        assert_eq!(ring.signing(), b"new");
        // The whitespace really is gone — not merely counted away.
        assert_eq!(ring.verifying().nth(1).unwrap(), b"old");
    }

    #[test]
    fn an_empty_spec_is_refused_rather_than_silently_signing_with_nothing() {
        assert!(KeyRing::parse("").is_err());
        assert!(KeyRing::parse("   ").is_err());
        assert!(KeyRing::parse(",,,").is_err());
        assert!(KeyRing::parse(" , ").is_err());
    }

    #[test]
    fn a_token_minted_under_a_dropped_key_stops_verifying() {
        let token =
            encode_bound("narrate", &json!({}), OLD, 200, "telegram", "acme", None).expect("fits");

        let during = KeyRing {
            keys: vec![NEW.to_vec(), OLD.to_vec()],
        };
        assert!(decode_bound_any(&token, &during, 200, "telegram", "acme").is_ok());

        let after = KeyRing::single(NEW);
        assert!(
            matches!(
                decode_bound_any(&token, &after, 200, "telegram", "acme"),
                Err(DecodeError::BadSignature)
            ),
            "dropping the key must close the window",
        );
    }

    #[test]
    fn the_ring_does_not_widen_the_tenant_binding() {
        // Every key on the ring still derives a per-tenant key, so a
        // ring is not a way to smuggle a foreign-tenant token through.
        let token = encode_bound("narrate", &json!({}), OLD, 200, "telegram", "globex", None)
            .expect("fits");
        let ring = KeyRing {
            keys: vec![NEW.to_vec(), OLD.to_vec()],
        };
        assert!(decode_bound_any(&token, &ring, 200, "telegram", "acme").is_err());
    }

    #[test]
    fn an_expired_token_reports_expiry_not_a_signature_mismatch() {
        // The MAC passes under the second key; only the body is stale.
        // Reporting `BadSignature` there would tell an operator their
        // rotation broke when in fact the token simply timed out.
        let long_ago = 1_000_000; // ~1970
        let token = encode_bound_at(
            "narrate",
            &json!({}),
            OLD,
            200,
            "telegram",
            "acme",
            long_ago,
        )
        .expect("fits");
        let ring = KeyRing {
            keys: vec![NEW.to_vec(), OLD.to_vec()],
        };
        match decode_bound_any(&token, &ring, 200, "telegram", "acme") {
            Err(DecodeError::Body(m)) => assert!(m.contains("expired"), "{m}"),
            other => panic!("expected an expiry verdict, got {other:?}"),
        }
    }

    #[test]
    fn unbound_tokens_rotate_on_the_same_ring() {
        // Report images and dashboard PNGs carry no tenant binding but
        // are signed with the same secret, so they must survive a
        // rotation too — otherwise every card image 404s on deploy.
        let token = encode_with_cap("__img", &json!({ "a": 1 }), OLD, 4096).expect("fits");
        let ring = KeyRing {
            keys: vec![NEW.to_vec(), OLD.to_vec()],
        };
        let (marker, _) = decode_with_cap_any(&token, &ring, 4096).expect("verifies");
        assert_eq!(marker, "__img");
    }

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
        let t = encode_bound(
            "narrate",
            &json!({"s": "a"}),
            KEY,
            CAP,
            "tg",
            "acme",
            Some(3600),
        )
        .unwrap();
        let (tool, args) = decode_bound(&t, KEY, CAP, "tg", "acme").unwrap();
        assert_eq!(tool, "narrate");
        assert_eq!(args["s"], "a");
    }

    #[test]
    fn another_tenant_cannot_use_it() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "tg", "acme", Some(3600)).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "tg", "globex").is_err());
    }

    #[test]
    fn an_unbound_legacy_token_is_refused() {
        // The compatibility decision, pinned: accepting these would keep
        // the replay open for every card minted before the binding
        // shipped, and unbound tokens never expire.
        let legacy = encode_with_cap("narrate", &json!({}), KEY, CAP).unwrap();
        assert!(decode_bound(&legacy, KEY, CAP, "tg", "acme").is_err());
        // ...but it still decodes on the unbound path, so nothing else breaks.
        assert!(decode_with_cap(&legacy, KEY, CAP).is_ok());
    }

    #[test]
    fn an_expired_token_is_refused() {
        // Built directly with a past expiry: with HOUR granularity a
        // `ttl_secs = 0` token still runs to the end of the current
        // hour, so a sleep cannot make one lapse inside a test.
        let args = json!({});
        let past = CompactBody {
            t: "narrate",
            a: &args,
            x: Some(now_secs().unwrap() / 3600 - 1),
        };
        let t = finish(past, &tenant_key(KEY, "tg", "acme"), CAP).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "tg", "acme").is_err());
    }

    #[test]
    fn a_ttl_rounds_up_so_a_token_never_dies_early() {
        // Hour granularity must never shorten the caller's TTL.
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "tg", "acme", Some(1)).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "tg", "acme").is_ok());
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
            "dc",
            "28c0071d-815c-4ace-a3b5-9a28bde005fd",
            Some(7 * 24 * 3600),
        )
        .expect("a realistic bound token fits Discord's budget");
        assert!(t.len() <= DISCORD_MAX_CUSTOM_ID, "got {} bytes", t.len());
    }

    /// Telegram's 64-byte `callback_data` is the tightest budget any
    /// adapter mints into, and a real unbound token already reaches it.
    /// A WIRE binding could never fit — carrying the tenant plus an
    /// expiry cost ~32 bytes — which is why the first attempt at this
    /// left Telegram and WhatsApp Cloud with the replay open.
    ///
    /// Deriving the key from the tenant costs nothing on the wire, so
    /// the binding fits everywhere. The expiry is the part still dropped
    /// at this budget; a stale token stays tenant-scoped, which is the
    /// property that matters.
    #[test]
    fn a_tenant_binding_fits_even_telegrams_budget() {
        let t = encode_bound(
            "narrate",
            &json!({ "subject": "alice" }),
            KEY,
            PLATFORM_MAX_CALLBACK_DATA,
            "tg",
            "28c0071d-815c-4ace-a3b5-9a28bde005fd",
            None,
        )
        .expect("a tenant-bound token fits Telegram when the expiry is dropped");
        assert!(
            t.len() <= PLATFORM_MAX_CALLBACK_DATA,
            "got {} bytes",
            t.len()
        );
        assert!(decode_bound(&t, KEY, PLATFORM_MAX_CALLBACK_DATA, "tg", "globex").is_err());
        assert!(
            decode_bound(
                &t,
                KEY,
                PLATFORM_MAX_CALLBACK_DATA,
                "tg",
                "28c0071d-815c-4ace-a3b5-9a28bde005fd"
            )
            .is_ok()
        );
    }

    /// An expiry-less token must not be mistaken for an expired one, and
    /// must still be refused for the wrong tenant.
    #[test]
    fn an_expiry_less_token_never_expires_but_stays_tenant_scoped() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "tg", "acme", None).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "tg", "acme").is_ok());
        assert!(decode_bound(&t, KEY, CAP, "tg", "globex").is_err());
    }

    #[test]
    fn a_bound_token_still_fails_a_wrong_key() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "tg", "acme", Some(3600)).unwrap();
        assert!(decode_bound(&t, b"other", CAP, "tg", "acme").is_err());
    }

    #[test]
    fn the_binding_costs_bytes_and_the_cap_still_applies() {
        // The tenant + expiry make the token longer; a cap that fitted
        // the unbound form may not fit the bound one, and that must
        // surface as OversizedToken rather than a silently dropped
        // binding.
        let unbound = encode_with_cap("narrate", &json!({}), KEY, CAP).unwrap();
        let bound =
            encode_bound("narrate", &json!({}), KEY, CAP, "tg", "acme", Some(3600)).unwrap();
        assert!(bound.len() > unbound.len());
        assert!(matches!(
            encode_bound(
                "narrate",
                &json!({}),
                KEY,
                unbound.len(),
                "tg",
                "acme",
                Some(3600)
            ),
            Err(EncodeError::OversizedToken { .. })
        ));
    }

    /// #250 F5: two adapters sharing one `correlation_key` — nothing
    /// refuses that today, and every fixture uses a single literal —
    /// must not accept each other's tokens. It matters most for the
    /// expiry-less tokens Telegram's budget forces: an immortal
    /// `callback_data` token would otherwise be a byte-valid Discord
    /// `custom_id`, re-creating the forever-capability one adapter over.
    #[test]
    fn a_token_from_one_platform_is_refused_on_another() {
        let t = encode_bound("narrate", &json!({}), KEY, CAP, "telegram", "acme", None).unwrap();
        assert!(decode_bound(&t, KEY, CAP, "telegram", "acme").is_ok());
        assert!(
            decode_bound(&t, KEY, CAP, "discord", "acme").is_err(),
            "same key, same tenant, different platform must not verify"
        );
    }
}
