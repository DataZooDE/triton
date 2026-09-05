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

    /// True when the bucket has refilled to capacity — i.e. it holds no
    /// throttle state anyone would lose. Used to pick an eviction victim
    /// that cannot be turned into a reset (#250).
    pub fn is_replenished(&self) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().expect("ratelimit mutex poisoned");
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.rate).min(self.burst);
        state.last_refill = now;
        state.tokens >= self.burst
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

/// Fair-share rate limit *within* an adapter (NFR-P-3 second
/// tier). PR 24 shipped the adapter-wide bucket as the DoS guard
/// (consumed before sender resolution); PR 28 layers a
/// per-tenant bucket on top so one noisy tenant can't starve
/// others sharing the same adapter quota.
///
/// Memory: one `TokenBucket` per distinct tenant that's been seen,
/// created lazily, capped at [`MAX_TENANT_BUCKETS`].
///
/// The cap is not decoration. This originally documented the
/// cardinality as "bounded by the manifest, not by inbound traffic",
/// which held for `sender_table` — tenants enumerated at boot — and was
/// false for FR-I-7 `upstream`, where an out-of-process resolver names
/// the tenant per request (#250). Validating the resolver reply at the
/// boundary keeps hostile values out, but a merely buggy resolver, or a
/// legitimately large tenant estate, still grows the map for the life of
/// the process. An invariant a type claims should hold in the type.
///
/// Eviction is oldest-first by last use, and deliberately does NOT reset
/// a bucket: see [`PerTenantBuckets::try_take`].
/// Ceiling on distinct tenant buckets held at once. Far above any real
/// estate, low enough that an unbounded key source cannot exhaust
/// memory.
pub const MAX_TENANT_BUCKETS: usize = 4096;

#[derive(Debug)]
pub struct PerTenantBuckets {
    rate: u32,
    burst: u32,
    buckets: std::sync::Mutex<std::collections::HashMap<String, Tracked>>,
}

#[derive(Debug)]
struct Tracked {
    bucket: TokenBucket,
    /// Last time this tenant was seen, for oldest-first eviction.
    last_used: Instant,
}

impl PerTenantBuckets {
    pub fn new(messages_per_sec: u32, burst: u32) -> Self {
        Self {
            rate: messages_per_sec,
            burst,
            buckets: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Try to consume one token from the bucket dedicated to
    /// `tenant`. Same return shape as `TokenBucket::try_take`:
    /// `Ok(())` on admission, `Err(retry_after_secs)` on refusal.
    /// The bucket for `tenant` is created on first use.
    /// Try to consume one token from the bucket dedicated to
    /// `tenant`. Same return shape as `TokenBucket::try_take`:
    /// `Ok(())` on admission, `Err(retry_after_secs)` on refusal.
    /// The bucket for `tenant` is created on first use.
    ///
    /// When the map is full, a bucket is evicted to make room — but only
    /// one that has refilled to capacity, and so holds no throttle state
    /// to lose. Evicting a DEPLETED bucket would be a reset: the tenant
    /// reappears with a full allowance. That is reachable, because an
    /// attacker who can name tenants floods the map with fresh names and
    /// pushes the throttled victim out — the eviction victim is not the
    /// attacker, as an earlier version of this comment wrongly claimed;
    /// a caller naming a new key each time is never the least recently
    /// used.
    ///
    /// So: evict the least recently used REPLENISHED bucket, and when
    /// every bucket is still owed something, refuse the newcomer rather
    /// than make room. Fail closed — under a flood where everyone is
    /// throttled, a new tenant waits.
    pub fn try_take(&self, tenant: &str) -> Result<(), f64> {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .expect("per-tenant rate-limit mutex poisoned");
        if buckets.len() >= MAX_TENANT_BUCKETS && !buckets.contains_key(tenant) {
            let victim = buckets
                .iter()
                .filter(|(k, t)| k.as_str() != tenant && t.bucket.is_replenished())
                .min_by_key(|(_, t)| t.last_used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(v) => {
                    buckets.remove(&v);
                }
                // Every tracked tenant is still owed tokens. Admitting
                // this one would mean evicting throttle state, which is
                // the bypass. Refuse instead.
                None => return Err(1.0),
            }
        }
        let tracked = buckets
            .entry(tenant.to_string())
            .or_insert_with(|| Tracked {
                bucket: TokenBucket::new(self.rate, self.burst),
                last_used: now,
            });
        tracked.last_used = now;
        tracked.bucket.try_take()
    }

    /// How many tenant buckets are currently held. For tests and
    /// diagnostics; never a decision input.
    pub fn tracked_tenants(&self) -> usize {
        self.buckets
            .lock()
            .expect("per-tenant rate-limit mutex poisoned")
            .len()
    }
}

/// Coalescing window for **anonymous** rejection audit (#249).
///
/// Not a rate limit: nothing is refused. A public inbound path answers an
/// unauthenticated probe with 401 and audits it, all before any rate-limit
/// token is consumed (the bucket is deliberately taken only after auth, so
/// a sprayer can't burn it). A background scanner therefore writes one
/// audit line per probe and the ring buffer evicts every real entry —
/// the history an operator tails is gone exactly when it is needed.
///
/// So the first rejection in a window emits **immediately** — an operator
/// debugging a genuine 401 must not wait out a window to learn why
/// (FR-AU / #219: a refusal says WHY in the line itself) — and the rest
/// are counted. The next one after the window closes emits carrying that
/// count, so the surviving line stays honest about what it stands for.
///
/// The count is best-effort by construction: a flood that STOPS inside a
/// window leaves its tail unreported, because nothing arrives to carry
/// the number out. Deliberate — no timer, no background task, nothing to
/// drain on shutdown (G-8). The exact total is never lost anyway: the
/// caller counts every rejection in metrics BEFORE consulting this, so
/// coalescing only ever costs per-request repetition in the log and the
/// ring buffer.
///
/// # Keys must be bounded by configuration, never by the request
///
/// `key` is hashed into a map that lives for the process, so anything
/// caller-controlled turns this into a memory-growth DoS. Callers key on
/// the PROTOCOL — a closed set fixed at compile time or by the manifest.
/// The obvious finer key, the tool name, is exactly the trap: the REST
/// adapter audits a rejection under `Path(name)` (the URL segment an
/// unauthenticated caller chooses) and MCP under a name off the JSON-RPC
/// body, so keying on it would let `/v1/tools/<random>` mint unbounded
/// buckets.
#[derive(Debug)]
pub struct RejectionWindow {
    window: std::time::Duration,
    state: Mutex<std::collections::HashMap<String, WindowState>>,
}

#[derive(Debug)]
struct WindowState {
    /// When the window currently in force opened (i.e. the last emission).
    opened: Instant,
    /// Rejections swallowed since that emission.
    suppressed: u64,
}

impl RejectionWindow {
    pub fn new(window: std::time::Duration) -> Self {
        Self {
            window,
            state: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Decide whether this rejection should be audited.
    ///
    /// `Some(n)` — emit, and say the line stands for `n` further
    /// rejections swallowed since the last one (`0` on the first).
    /// `None` — suppressed and counted.
    ///
    /// A zero-length window disables coalescing entirely (every
    /// rejection emits, `suppressed` always `0`), which is the escape
    /// hatch for a deployment that wants the un-coalesced stream.
    pub fn admit(&self, key: &str) -> Option<u64> {
        if self.window.is_zero() {
            return Some(0);
        }
        let now = Instant::now();
        let mut state = self.state.lock().expect("rejection-window mutex poisoned");
        match state.get_mut(key) {
            // Window still in force: swallow and count.
            Some(w) if now.duration_since(w.opened) < self.window => {
                w.suppressed += 1;
                None
            }
            // Window lapsed: emit, reporting what it swallowed.
            Some(w) => {
                let n = w.suppressed;
                w.opened = now;
                w.suppressed = 0;
                Some(n)
            }
            // First rejection for this key: emit at once.
            None => {
                state.insert(
                    key.to_string(),
                    WindowState {
                        opened: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
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

    #[test]
    fn per_tenant_buckets_are_independent() {
        // PR 28: NFR-P-3 second-tier fair share. Two tenants
        // hitting the same adapter shouldn't starve each other.
        let b = PerTenantBuckets::new(0, 2); // rate=0 so no refill
        assert!(b.try_take("alpha").is_ok());
        assert!(b.try_take("alpha").is_ok());
        // alpha is empty.
        assert!(b.try_take("alpha").is_err());
        // beta has a fresh bucket.
        assert!(b.try_take("beta").is_ok());
        assert!(b.try_take("beta").is_ok());
        assert!(b.try_take("beta").is_err());
        // alpha hasn't been refilled in the meantime.
        assert!(b.try_take("alpha").is_err());
    }

    /// #250: the type's doc-comment promised "the cardinality is bounded
    /// by the manifest, not by inbound traffic". That was true for
    /// `sender_table`, where tenants are enumerated at boot — and false
    /// for FR-I-7 `upstream`, where an out-of-process resolver names the
    /// tenant per request. Validation at the resolver boundary keeps
    /// hostile values out, but a resolver that is merely buggy, or a
    /// legitimately large tenant estate, still grows the map for the
    /// life of the process. The invariant has to hold in the type, not
    /// only in its callers.
    #[test]
    fn the_bucket_map_is_bounded_regardless_of_what_callers_pass() {
        let b = PerTenantBuckets::new(100, 100);
        for i in 0..(MAX_TENANT_BUCKETS * 3) {
            let _ = b.try_take(&format!("tenant-{i}"));
        }
        assert!(
            b.tracked_tenants() <= MAX_TENANT_BUCKETS,
            "map grew to {} entries",
            b.tracked_tenants()
        );
    }

    /// Eviction must not become a bypass: a tenant that has just been
    /// refused must not get a fresh, full bucket by pushing itself out
    /// of the map and back in.
    #[test]
    fn eviction_does_not_hand_out_a_fresh_bucket_to_a_throttled_tenant() {
        let b = PerTenantBuckets::new(0, 1);
        assert!(b.try_take("victim").is_ok());
        assert!(b.try_take("victim").is_err(), "bucket now empty");
        // Flood the map so `victim` is a candidate for eviction.
        for i in 0..(MAX_TENANT_BUCKETS * 2) {
            let _ = b.try_take(&format!("noise-{i}"));
        }
        assert!(
            b.try_take("victim").is_err(),
            "a throttled tenant must not regain a full bucket by \
             flooding the map"
        );
    }

    #[test]
    fn per_tenant_buckets_lazy_init_each_tenant() {
        // First call for a new tenant MUST admit (we shouldn't
        // accidentally pre-deplete a fresh bucket).
        let b = PerTenantBuckets::new(0, 1);
        assert!(b.try_take("first").is_ok());
        assert!(b.try_take("second").is_ok());
        assert!(b.try_take("third").is_ok());
        // Each of those buckets is now empty.
        assert!(b.try_take("first").is_err());
        assert!(b.try_take("second").is_err());
        assert!(b.try_take("third").is_err());
    }

    // ---- #249 RejectionWindow ---------------------------------------

    #[test]
    fn first_rejection_emits_immediately_then_the_rest_are_counted() {
        let w = RejectionWindow::new(Duration::from_secs(3600));
        // An operator debugging a real 401 sees it NOW, not after the
        // window (#219).
        assert_eq!(w.admit("rest"), Some(0));
        for _ in 0..5 {
            assert_eq!(w.admit("rest"), None, "swallowed inside the window");
        }
    }

    #[test]
    fn a_reopened_window_reports_what_it_swallowed_then_resets() {
        let w = RejectionWindow::new(Duration::from_millis(60));
        assert_eq!(w.admit("rest"), Some(0));
        for _ in 0..4 {
            assert_eq!(w.admit("rest"), None);
        }
        thread::sleep(Duration::from_millis(80));
        assert_eq!(w.admit("rest"), Some(4), "reports the 4 it swallowed");
        // The counter resets — the next window doesn't re-report them.
        thread::sleep(Duration::from_millis(80));
        assert_eq!(w.admit("rest"), Some(0));
    }

    #[test]
    fn windows_are_independent_per_key() {
        let w = RejectionWindow::new(Duration::from_secs(3600));
        assert_eq!(w.admit("rest"), Some(0));
        assert_eq!(w.admit("rest"), None);
        // A different protocol has its own window: one adapter's scanner
        // must not silence another adapter's first refusal.
        assert_eq!(w.admit("messenger:msteams"), Some(0));
        assert_eq!(w.admit("a2a"), Some(0));
    }

    #[test]
    fn a_zero_window_disables_coalescing() {
        // The escape hatch for a deployment that wants every line.
        let w = RejectionWindow::new(Duration::ZERO);
        for _ in 0..10 {
            assert_eq!(w.admit("rest"), Some(0));
        }
    }
}
