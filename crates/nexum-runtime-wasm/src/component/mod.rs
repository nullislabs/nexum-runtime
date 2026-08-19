//! The [`Components`] backend bundle and the builder that assembles it.

mod builder;

pub use builder::{BuildError, ComponentsBuilder};

use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_logs::LogPipeline;

/// Owned bundle of shared backends threaded into every module store; cheap to
/// clone.
pub struct Components<T: RuntimeTypes> {
    /// Shared JSON-RPC pool, one provider per configured chain.
    pub chain: ProviderPool,
    /// Shared store backend; each module sees only its own namespace.
    pub store: T::Store,
    /// Shared log pipeline.
    pub logs: LogPipeline,
}

impl<T: RuntimeTypes> Clone for Components<T> {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            store: self.store.clone(),
            logs: self.logs.clone(),
        }
    }
}
