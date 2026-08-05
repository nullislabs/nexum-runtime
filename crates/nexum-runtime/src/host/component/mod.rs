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
    pub chain: ProviderPool,
    pub store: T::Store,
    /// Extension backends (the lattice `Ext` payload).
    pub ext: T::Ext,
    /// Shared log pipeline.
    pub logs: crate::host::logs::LogPipeline,
}

impl<T: RuntimeTypes> Clone for Components<T> {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            store: self.store.clone(),
            ext: self.ext.clone(),
            logs: self.logs.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::local_store_redb::{LocalStore, ModuleStore};

    /// Core-only lattice (no extension payload).
    #[derive(Clone, Copy, Default)]
    struct CoreTypes;

    impl crate::sealed::SealedRuntimeTypes for CoreTypes {}

    impl RuntimeTypes for CoreTypes {
        type Store = LocalStore;
        type Ext = ();
    }

    fn store<T: StateStore>() {}
    fn handle<T: StateHandle>() {}
    fn lattice<T: RuntimeTypes>() {}

    #[test]
    fn concrete_backends_satisfy_the_traits() {
        store::<LocalStore>();
        handle::<ModuleStore>();
        lattice::<CoreTypes>();
    }
}
