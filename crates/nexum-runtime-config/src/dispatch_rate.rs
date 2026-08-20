use std::num::NonZeroU32;
use std::time::Instant;

/// A literal as non-zero; a zero fails the build.
const fn nz(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(v) => v,
        None => panic!("zero constant"),
    }
}

/// Per-module token-bucket thresholds from `[limits.dispatch]`.
#[derive(Debug, Clone, Copy)]
pub struct DispatchRatePolicy {
    /// The burst allowance.
    pub capacity: NonZeroU32,
    /// The sustained ceiling, in dispatches per second.
    pub refill_per_sec: NonZeroU32,
}

impl DispatchRatePolicy {
    /// Pair a burst allowance with the rate that refills it.
    pub const fn new(capacity: NonZeroU32, refill_per_sec: NonZeroU32) -> Self {
        Self {
            capacity,
            refill_per_sec,
        }
    }
}

impl Default for DispatchRatePolicy {
    fn default() -> Self {
        Self::new(DEFAULT_DISPATCH_BURST, DEFAULT_DISPATCH_REFILL_PER_SEC)
    }
}

/// Default burst allowance.
pub const DEFAULT_DISPATCH_BURST: NonZeroU32 = nz(256);

/// Default sustained ceiling, in dispatches per second.
pub const DEFAULT_DISPATCH_REFILL_PER_SEC: NonZeroU32 = nz(128);

/// Default host log rate: a 256-record burst refilled at 128 records per
/// second, one bucket per component.
pub const DEFAULT_LOG_RATE: DispatchRatePolicy = DispatchRatePolicy::new(nz(256), nz(128));

/// Token-bucket state for one policy holder; fractional tokens, starts
/// full. Time is injected, so the caller decides which clock the bound is
/// measured on.
#[derive(Debug)]
pub struct TokenBucket {
    policy: DispatchRatePolicy,
    /// Current tokens in `[0, capacity]`; fractional so slow refill is not lost.
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// A bucket that starts full at `policy.capacity`, as of `now`.
    pub fn new(policy: DispatchRatePolicy, now: Instant) -> Self {
        Self {
            policy,
            tokens: f64::from(policy.capacity.get()),
            last_refill: now,
        }
    }

    /// Refill for elapsed time, then consume one token; `true` allowed,
    /// `false` over-rate.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        let capacity = f64::from(self.policy.capacity.get());
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens =
            (self.tokens + elapsed * f64::from(self.policy.refill_per_sec.get())).min(capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn default_is_production_constants() {
        let p = DispatchRatePolicy::default();
        assert_eq!(p.capacity, DEFAULT_DISPATCH_BURST);
        assert_eq!(p.refill_per_sec, DEFAULT_DISPATCH_REFILL_PER_SEC);
    }

    #[test]
    fn bucket_starts_full_and_allows_a_burst_up_to_capacity() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(DispatchRatePolicy::new(nz(3), nz(1)), now);
        // Three dispatches in the same instant clear the burst allowance.
        assert!(bucket.try_acquire(now));
        assert!(bucket.try_acquire(now));
        assert!(bucket.try_acquire(now));
        // The fourth over-rate event in the same instant is dropped.
        assert!(!bucket.try_acquire(now));
    }

    #[test]
    fn empty_bucket_refills_over_time() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(DispatchRatePolicy::new(nz(2), nz(4)), start);
        // Drain the burst.
        assert!(bucket.try_acquire(start));
        assert!(bucket.try_acquire(start));
        assert!(!bucket.try_acquire(start), "burst exhausted");
        // 4 tokens/s means one token is back after 250 ms.
        let later = start + Duration::from_millis(250);
        assert!(bucket.try_acquire(later), "one token refilled after 250ms");
        assert!(!bucket.try_acquire(later), "only one token had refilled");
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(DispatchRatePolicy::new(nz(2), nz(100)), start);
        assert!(bucket.try_acquire(start));
        assert!(bucket.try_acquire(start));
        // A long idle would refill 100 tokens/s, but the bucket caps at
        // capacity: only `capacity` dispatches are allowed back-to-back.
        let much_later = start + Duration::from_secs(10);
        assert!(bucket.try_acquire(much_later));
        assert!(bucket.try_acquire(much_later));
        assert!(
            !bucket.try_acquire(much_later),
            "burst is capped at capacity, not the whole idle refill",
        );
    }

    /// A flooding source is throttled while an independent source is served.
    #[test]
    fn one_flooding_bucket_does_not_starve_another() {
        let now = Instant::now();
        let policy = DispatchRatePolicy::new(nz(2), nz(1));
        let mut flooder = TokenBucket::new(policy, now);
        let mut neighbour = TokenBucket::new(policy, now);

        // Hammer the flooder in a single instant: the first `capacity`
        // dispatches pass, the rest are dropped.
        let mut allowed = 0;
        for _ in 0..100 {
            if flooder.try_acquire(now) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 2, "flooder is throttled to its burst allowance");
        assert!(!flooder.try_acquire(now), "flooder stays throttled");

        // The neighbour's bucket is untouched by the flood: it still
        // serves its own full burst.
        assert!(neighbour.try_acquire(now));
        assert!(neighbour.try_acquire(now));
    }
}
