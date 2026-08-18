//! [`ComponentsBuilder`] assembles the per-backend [`ComponentBuilder`]s and
//! the log pipeline into a [`Components`] bundle.

use nexum_runtime_logs::LogPipelineBuilder;

use crate::error::BoxError;
use crate::host::component::{BuilderContext, ComponentBuilder, Components, RuntimeTypes};
use crate::host::logs::LogPipeline;
use crate::host::provider_pool::ProviderPool;

/// Names the component slot whose build failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// The chain backend builder failed.
    #[error("build the chain backend: {0}")]
    Chain(#[source] BoxError),
    /// The store backend builder failed.
    #[error("build the store backend: {0}")]
    Store(#[source] BoxError),
    /// The log pipeline builder failed.
    #[error("build the log pipeline: {0}")]
    Logs(#[source] BoxError),
}

/// Assembles the core and log-pipeline builders into a [`Components`]
/// bundle; the logs slot defaults to [`LogPipelineBuilder`].
pub struct ComponentsBuilder<C, S, L = LogPipelineBuilder> {
    /// Builds the chain backend.
    pub chain: C,
    /// Builds the store backend ([`RuntimeTypes::Store`]).
    pub store: S,
    /// Builds the shared [`LogPipeline`].
    pub logs: L,
}

impl<C, S> ComponentsBuilder<C, S> {
    /// Create a new [`ComponentsBuilder`] with the default log pipeline.
    pub fn new(chain: C, store: S) -> Self {
        Self {
            chain,
            store,
            logs: LogPipelineBuilder,
        }
    }
}

impl<C, S, L> ComponentsBuilder<C, S, L> {
    /// Replace the log pipeline builder.
    #[must_use]
    pub fn with_logs<L2>(self, logs: L2) -> ComponentsBuilder<C, S, L2> {
        ComponentsBuilder {
            chain: self.chain,
            store: self.store,
            logs,
        }
    }

    /// Drive each builder against `ctx` and bundle the backends; a failing
    /// sub-build returns the [`BuildError`] naming that slot.
    pub async fn build<T>(self, ctx: &BuilderContext<'_>) -> Result<Components<T>, BuildError>
    where
        T: RuntimeTypes,
        C: ComponentBuilder<Output = ProviderPool>,
        S: ComponentBuilder<Output = T::Store>,
        L: ComponentBuilder<Output = LogPipeline>,
    {
        let chain = self.chain.build(ctx).await.map_err(BuildError::Chain)?;
        let store = self.store.build(ctx).await.map_err(BuildError::Store)?;
        let logs = self.logs.build(ctx).await.map_err(BuildError::Logs)?;
        Ok(Components { chain, store, logs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_config::EngineConfig;
    use crate::host::component::{LocalStoreBuilder, ProviderPoolBuilder};
    use crate::preset::CoreRuntime;

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
