//! The store's resource limits, read on the way past.

use wasmtime::{Error, ResourceLimiter, Result, StoreLimits};

/// A [`StoreLimits`] that also records the linear memory size it last allowed.
///
/// Every method forwards; a trait default left in place would silently
/// replace the wrapped instance, table and memory caps with wasmtime's own.
/// Wasm linear memory never shrinks, so the last allowed `desired` is the
/// current size.
pub struct ObservedLimits {
    inner: StoreLimits,
    memory_bytes: usize,
}

impl ObservedLimits {
    /// Wraps `inner`, observing it rather than replacing it.
    pub fn new(inner: StoreLimits) -> Self {
        Self {
            inner,
            memory_bytes: 0,
        }
    }

    /// Linear memory this store has grown to, in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }
}

impl ResourceLimiter for ObservedLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        let allow = self.inner.memory_growing(current, desired, maximum)?;
        if allow {
            self.memory_bytes = desired;
        }
        Ok(allow)
    }

    fn memory_grow_failed(&mut self, error: Error) -> Result<()> {
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: Error) -> Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 64 * 1024;

    fn observed() -> ObservedLimits {
        ObservedLimits::new(
            wasmtime::StoreLimitsBuilder::new()
                .memory_size(2 * PAGE)
                .table_elements(16)
                .instances(3)
                .tables(4)
                .memories(5)
                .build(),
        )
    }

    #[test]
    fn the_last_allowed_growth_is_the_memory_reading() {
        let mut limits = observed();
        assert_eq!(limits.memory_bytes(), 0);
        assert!(limits.memory_growing(0, PAGE, None).expect("allowed"));
        assert_eq!(limits.memory_bytes(), PAGE);
        assert!(
            limits
                .memory_growing(PAGE, 2 * PAGE, None)
                .expect("allowed")
        );
        assert_eq!(limits.memory_bytes(), 2 * PAGE);
    }

    #[test]
    fn a_refused_growth_leaves_the_reading_at_the_size_the_module_holds() {
        let mut limits = observed();
        assert!(limits.memory_growing(0, PAGE, None).expect("allowed"));
        assert!(
            !limits
                .memory_growing(PAGE, 3 * PAGE, None)
                .expect("refused")
        );
        assert_eq!(limits.memory_bytes(), PAGE);
    }

    /// The wrapped ceiling, not this wrapper, is what refuses.
    #[test]
    fn the_memory_ceiling_refuses_what_it_refused_before() {
        let mut limits = observed();
        assert!(!limits.memory_growing(0, 3 * PAGE, None).expect("refused"));
    }

    #[test]
    fn the_table_ceiling_still_refuses_past_its_limit() {
        let mut limits = observed();
        assert!(limits.table_growing(0, 16, None).expect("allowed"));
        assert!(!limits.table_growing(16, 17, None).expect("refused"));
    }

    /// A wrapper leaving these to the trait default would report wasmtime's
    /// 10,000 rather than the operator's configuration.
    #[test]
    fn the_instance_table_and_memory_counts_are_the_wrapped_ones() {
        let limits = observed();
        assert_eq!(
            (limits.instances(), limits.tables(), limits.memories()),
            (3, 4, 5)
        );
    }

    /// `trap_on_grow_failure` turns a refusal into a trap, which only the
    /// wrapped limiter knows; the trait default swallows both errors.
    #[test]
    fn a_grow_failure_is_reported_by_the_wrapped_limiter() {
        let mut limits = ObservedLimits::new(
            wasmtime::StoreLimitsBuilder::new()
                .trap_on_grow_failure(true)
                .build(),
        );
        let failure = || Error::from(std::io::Error::other("grow"));
        assert!(limits.memory_grow_failed(failure()).is_err());
        assert!(limits.table_grow_failed(failure()).is_err());
    }
}
