use std::num::NonZeroU32;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_production_constants() {
        let p = DispatchRatePolicy::default();
        assert_eq!(p.capacity, DEFAULT_DISPATCH_BURST);
        assert_eq!(p.refill_per_sec, DEFAULT_DISPATCH_REFILL_PER_SEC);
    }
}
