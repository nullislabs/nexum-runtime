//! Runtime presets: one preset bundles a lattice, its component builders,
//! extensions, and add-ons, so an embedder launches with
//! `RuntimeBuilder::new(cfg).runtime::<Preset>().launch()`. A preset carrying
//! pre-built backends or non-static extensions binds by value through
//! [`RuntimeBuilder::with_runtime`](crate::builder::RuntimeBuilder::with_runtime).
//! [`CoreRuntime`] is the domain-free default: a chain provider pool and a
//! local redb store, with the Prometheus add-on.

use std::sync::Arc;

use nexum_runtime_api::{ComponentBuilder, Extension, RuntimeTypes};
use nexum_runtime_chain::{ProviderPool, ProviderPoolBuilder};
use nexum_runtime_logs::{LogPipeline, LogPipelineBuilder};
use nexum_runtime_store::{LocalStore, LocalStoreBuilder};
use nexum_runtime_wasm::{ComponentsBuilder, HostState};

use crate::addons::{AddOns, PrometheusAddOn};
use crate::engine_config::EngineConfig;

/// A bundled runtime assembly: the [`RuntimeTypes`] lattice plus the component
/// builders, extensions, and add-ons the launcher needs.
///
/// The marker bound is reserved for semver evolution: a preset opts in by
/// also implementing it.
pub trait Runtime: crate::sealed::SealedRuntime {
    /// The lattice the preset assembles.
    type Types: RuntimeTypes<State = HostState<Self::Types>>;
    /// Builds the concrete chain [`ProviderPool`].
    type ChainBuilder: ComponentBuilder<Output = ProviderPool>;
    /// Builds the store backend ([`RuntimeTypes::Store`]).
    type StoreBuilder: ComponentBuilder<Output = <Self::Types as RuntimeTypes>::Store>;
    /// Builds the shared [`LogPipeline`].
    type LogsBuilder: ComponentBuilder<Output = LogPipeline>;

    /// Component builders that open the backends at launch; consumes the
    /// preset, so a value-bound preset hands over owned, pre-built backends.
    #[must_use]
    fn components(
        self,
    ) -> ComponentsBuilder<Self::ChainBuilder, Self::StoreBuilder, Self::LogsBuilder>;

    /// The cross-cutting add-ons installed before the engine boots.
    fn add_ons(&self) -> AddOns;

    /// Extensions the preset launches with, derived from config. Empty by
    /// default;
    /// [`PresetBuilder::with_extensions`](crate::builder::PresetBuilder::with_extensions)
    /// appends on top.
    fn extensions(&self, config: &EngineConfig) -> Vec<Arc<dyn Extension<Self::Types>>> {
        let _ = config;
        Vec::new()
    }
}

/// The domain-free default preset: a chain provider pool and a local redb
/// store, with the Prometheus add-on. Doubles as its own [`RuntimeTypes`]
/// lattice.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreRuntime;

impl crate::sealed::SealedRuntimeTypes for CoreRuntime {}
impl crate::sealed::SealedRuntime for CoreRuntime {}

impl RuntimeTypes for CoreRuntime {
    type State = HostState<Self>;
    type Store = LocalStore;
}

impl Runtime for CoreRuntime {
    type Types = Self;
    type ChainBuilder = ProviderPoolBuilder;
    type StoreBuilder = LocalStoreBuilder;
    type LogsBuilder = LogPipelineBuilder;

    fn components(
        self,
    ) -> ComponentsBuilder<ProviderPoolBuilder, LocalStoreBuilder, LogPipelineBuilder> {
        ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
    }

    fn add_ons(&self) -> AddOns {
        vec![Box::new(PrometheusAddOn)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexum_runtime_api::{BuilderContext, StateHandle, StateStore};
    use nexum_runtime_store::ModuleStore;

    fn store<T: StateStore>() {}
    fn handle<T: StateHandle>() {}
    fn lattice<T: RuntimeTypes>() {}

    #[test]
    fn concrete_backends_satisfy_the_traits() {
        store::<LocalStore>();
        handle::<ModuleStore>();
        lattice::<CoreRuntime>();
    }

    /// Opens the core backends end-to-end against a fresh data directory.
    #[tokio::test]
    async fn components_builder_opens_the_core_backends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("nested-state");
        let config = EngineConfig::default();
        let tasks = nexum_tasks::TaskManager::new();
        let executor = tasks.executor();
        let ctx = BuilderContext {
            config: &config,
            data_dir: &data_dir,
            executor: &executor,
        };

        let components = ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
            .build::<CoreRuntime>(&ctx)
            .await
            .expect("build core components");

        // The store builder created the data directory eagerly.
        assert!(data_dir.is_dir(), "data directory created by the build");
        assert!(
            data_dir.join("local-store.redb").is_file(),
            "redb store opened under the data directory",
        );
        // The bundle carries a live in-memory log pipeline.
        let _ = &components.logs;
    }

    /// `with_logs` substitutes the log pipeline builder: the bundle carries
    /// the exact pipeline the custom builder yields.
    #[tokio::test]
    async fn with_logs_substitutes_the_pipeline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = EngineConfig::default();
        let tasks = nexum_tasks::TaskManager::new();
        let executor = tasks.executor();
        let ctx = BuilderContext {
            config: &config,
            data_dir: dir.path(),
            executor: &executor,
        };

        let custom = LogPipeline::in_memory(config.limits.logs);
        let components = ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
            .with_logs(crate::test_utils::Prebuilt(custom.clone()))
            .build::<CoreRuntime>(&ctx)
            .await
            .expect("build with a custom log pipeline");

        assert!(
            std::sync::Arc::ptr_eq(&components.logs.router(), &custom.router()),
            "bundle carries the substituted pipeline",
        );
    }
}
