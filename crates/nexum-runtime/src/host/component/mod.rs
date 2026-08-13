//! Backend component traits: the seam between the WIT host impls and the
//! concrete capability backends, tied together by the [`RuntimeTypes`]
//! lattice. The chain backend is the concrete [`ProviderPool`].

mod builder;
mod runtime_types;
mod state;

pub use builder::{
    BuildError, BuilderContext, ComponentBuilder, ComponentsBuilder, LocalStoreBuilder,
    LogPipelineBuilder, ProviderPoolBuilder,
};
pub use runtime_types::{Handle, RuntimeTypes};
pub use state::{StateHandle, StateStore};

/// Permitted read surface, re-exported from `nexum-world`.
pub use nexum_world::ChainMethod;

use crate::host::provider_pool::ProviderPool;

/// Owned bundle of shared backends threaded into every module store; cheap to
/// clone.
pub struct Components<T: RuntimeTypes> {
    /// Shared JSON-RPC provider pool, one provider per configured chain.
    pub chain: ProviderPool,
    /// Shared store backend; each module sees only its own namespace.
    pub store: T::Store,
    /// Shared log pipeline.
    pub logs: crate::host::logs::LogPipeline,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::local_store_redb::{LocalStore, ModuleStore};
    use crate::preset::CoreRuntime;

    fn store<T: StateStore>() {}
    fn handle<T: StateHandle>() {}
    fn lattice<T: RuntimeTypes>() {}

    #[test]
    fn concrete_backends_satisfy_the_traits() {
        store::<LocalStore>();
        handle::<ModuleStore>();
        lattice::<CoreRuntime>();
    }
}
