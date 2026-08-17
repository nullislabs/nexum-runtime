//! Supervisor module restart policy.
//!
//! On a trap in `on_event` the supervisor marks the module dead and schedules
//! a restart with exponential backoff; the next eligible dispatch retries, and
//! a successful call resets the failure counter.
//!
//! | failure_count | backoff delay |
//! |---|---|
//! | 1 | 0.5s - 1s |
//! | 2 | 1s - 2s |
//! | 3 | 2s - 4s |
//! | ... | doubles |
//! | 10+ | capped at 150s - 300s |
//!
//! The delay is drawn from the upper half of the doubling curve by a hash of
//! `(seed, failure_count)`, so modules that failed on one shared outage do
//! not retry in lockstep, yet a given `(seed, failure_count)` always yields
//! the same delay.
//!
//! State is in-memory per process; it does not persist across restarts.

use std::hash::{BuildHasher as _, RandomState};
use std::sync::OnceLock;
use std::time::Duration;

/// Hard cap on the restart backoff.
pub const RESTART_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Seed for [`backoff_for`]: FNV-1a over a caller identity, mixed with a
/// per-process nonce. Identities decorrelate the modules of one engine; the
/// nonce decorrelates a fleet running one config against one provider.
pub fn jitter_seed(identity: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^ process_nonce()
}

fn process_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    // `RandomState` is the randomness source std already carries.
    *NONCE.get_or_init(|| RandomState::new().hash_one(0u64))
}

/// Backoff before the next restart after `failure_count` consecutive traps.
/// `0` returns `Duration::ZERO` (steady state, callable unconditionally);
/// `>= 1` doubles from a 1 s base, capped at 5 min, jittered into the upper
/// half of the base by `seed`. Deterministic per `(seed, failure_count)`.
pub fn backoff_for(failure_count: u32, seed: u64) -> Duration {
    if failure_count == 0 {
        return Duration::ZERO;
    }
    // The .min(9) keeps the shift from overflowing on absurd counts.
    let shift = failure_count.saturating_sub(1).min(9);
    let base = Duration::from_secs(1u64 << shift).min(RESTART_MAX_BACKOFF);
    let half_ms = base.as_millis() as u64 / 2;
    let jitter_ms = splitmix64(seed ^ u64::from(failure_count)) % (half_ms + 1);
    Duration::from_millis(half_ms + jitter_ms)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_state_is_zero() {
        assert_eq!(backoff_for(0, 7), Duration::ZERO);
    }

    #[test]
    fn delay_stays_in_the_upper_half_of_the_doubling_curve() {
        for seed in [0, 1, jitter_seed("mod-a"), u64::MAX] {
            // Count 9 is base 256 s; the 300 s cap engages only at 10+.
            for (count, base_ms) in [
                (1, 1_000),
                (2, 2_000),
                (3, 4_000),
                (5, 16_000),
                (9, 256_000),
            ] {
                let delay = backoff_for(count, seed).as_millis() as u64;
                assert!(
                    (base_ms / 2..=base_ms).contains(&delay),
                    "count {count} seed {seed}: {delay}ms outside [{}, {base_ms}]",
                    base_ms / 2,
                );
            }
        }
    }

    #[test]
    fn same_seed_and_count_is_deterministic() {
        let seed = jitter_seed("mod-a");
        assert_eq!(backoff_for(4, seed), backoff_for(4, seed));
    }

    #[test]
    fn distinct_seeds_decorrelate() {
        let a = jitter_seed("mod-a");
        let b = jitter_seed("mod-b");
        assert_ne!(a, b);
        assert!(
            (1..=8).any(|count| backoff_for(count, a) != backoff_for(count, b)),
            "two modules share the whole schedule",
        );
    }

    #[test]
    fn caps_at_five_minutes() {
        for seed in [0, jitter_seed("mod-a"), u64::MAX] {
            for count in [10, 20, u32::MAX] {
                let delay = backoff_for(count, seed);
                assert!(delay <= RESTART_MAX_BACKOFF, "{count}/{seed}: {delay:?}");
                assert!(
                    delay >= RESTART_MAX_BACKOFF / 2,
                    "{count}/{seed}: {delay:?}"
                );
            }
        }
    }
}
