//! v0.2 PR 24 — per-adapter token-bucket rate limiter.
//!
//! Manifests declare `rate_limit.messages_per_sec` and `burst`
//! per chat adapter. NFR-P-3 says Triton enforces them so a
//! noisy bot can't saturate the dispatcher.
//!
//! This is the simplest classical token bucket:
//!   * `burst` tokens fill the bucket at boot.
//!   * Tokens refill at `messages_per_sec`/sec, capped at
//!     `burst`.
//!   * Each accepted inbound consumes one token.
//!
//! Per-tenant fair-share is a future enhancement; PR 24 scopes
//! to per-adapter only (the manifest field is per-adapter). The
//! interior `Mutex<State>` is fine for substrate-scale traffic —
//! Tokio's contention math says a single uncontended take/release
//! is ~50 ns, well below the per-request budget.

use std::sync::Mutex;
use std::time::Instant;

/// A token bucket bound to a single adapter. Hand it the manifest
/// `messages_per_sec` and `burst` at boot, then call
/// [`Self::try_take`] on every inbound. `Ok(())` means the
/// request is admitted; `Err(_)` means the bucket was empty and
/// the adapter should refuse with `phase: rejected,
/// result: error:ratelimit`.
#[derive(Debug)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(messages_per_sec: u32, burst: u32) -> Self {
        let burst_f = burst.max(1) as f64;
        Self {
            rate: messages_per_sec as f64,
            burst: burst_f,
            state: Mutex::new(State {
                tokens: burst_f,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Try to consume one token from the bucket. Returns `Ok(())`
    /// on admission, `Err(retry_after_secs)` on refusal with the
    /// number of seconds the caller would have to wait for a
    /// token to refill.
    pub fn try_take(&self) -> Result<(), f64> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("ratelimit mutex poisoned");
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.rate).min(self.burst);
        state.last_refill = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            Ok(())
        } else {
            // How long until one token? Useful for Retry-After /
            // tracing::warn. Avoid div-by-zero on a misconfigured
            // 0-rate bucket (manifest validator rejects this, but
            // belt-and-braces).
            let retry = if self.rate > 0.0 {
                (1.0 - state.tokens) / self.rate
            } else {
                f64::INFINITY
            };
            Err(retry)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn burst_is_absorbed_then_rejected() {
        let b = TokenBucket::new(0, 3); // rate=0 so no refill
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_ok());
        // Fourth take past the burst with no refill → reject.
        assert!(b.try_take().is_err());
    }

    #[test]
    fn refill_admits_after_elapsed_time() {
        let b = TokenBucket::new(100, 1); // 100/sec, burst 1
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_err());
        thread::sleep(Duration::from_millis(15)); // refill 1.5 tokens
        assert!(
            b.try_take().is_ok(),
            "expected refill after 15 ms at 100/sec"
        );
    }

    #[test]
    fn retry_after_is_positive_on_empty_bucket() {
        let b = TokenBucket::new(2, 1); // 2/sec, burst 1
        assert!(b.try_take().is_ok());
        match b.try_take() {
            Ok(_) => panic!("should have been rejected"),
            Err(retry) => {
                assert!(retry > 0.0);
                // Time-to-1-token at 2/sec ≈ 0.5 s; the bucket
                // already has some fractional refill from the
                // first try_take, so the headline value is just
                // "positive and finite".
                assert!(retry.is_finite());
            }
        }
    }

    #[test]
    fn zero_burst_still_admits_at_least_one_request_per_window() {
        // Manifest validator should refuse burst:0 but defensive
        // here: bucket clamps burst to 1 so a misconfigured (or
        // mid-migration) deploy still serves at least one in.
        let b = TokenBucket::new(1, 0);
        assert!(b.try_take().is_ok());
        assert!(b.try_take().is_err());
    }
}
