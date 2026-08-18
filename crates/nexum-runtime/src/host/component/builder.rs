//! Per-component builders. Each core backend is a [`ComponentBuilder`];
//! [`ComponentsBuilder`] assembles the core seams and the log pipeline into a
//! [`Components`] bundle.

use std::future::Future;
use std::path::Path;

use nexum_tasks::TaskExecutor;

use crate::error::BoxError;
use crate::host::component::{Components, RuntimeTypes};
use crate::host::local_store_redb::LocalStore;
use crate::host::logs::LogPipeline;
use crate::host::provider_pool::ProviderPool;

/// Shared inputs every component builder reads.
pub struct BuilderContext<'a> {
    /// The loaded engine config.
    pub config: &'a crate::engine_config::EngineConfig,
    /// Directory backends root their on-disk state at.
    pub data_dir: &'a Path,
    /// Runs blocking open work off the async executor.
    pub executor: &'a TaskExecutor,
}

/// Builds one runtime backend from the shared [`BuilderContext`].
pub trait ComponentBuilder {
    /// The backend this builder produces.
    type Output;

    /// Open the backend, consuming the builder.
    fn build(
        self,
        ctx: &BuilderContext<'_>,
    ) -> impl Future<Output = Result<Self::Output, BoxError>> + Send;
}

/// Builds the [`ProviderPool`] from `[chains]`.
pub struct ProviderPoolBuilder;

impl ComponentBuilder for ProviderPoolBuilder {
    type Output = ProviderPool;

    async fn build(self, ctx: &BuilderContext<'_>) -> Result<ProviderPool, BoxError> {
        ProviderPool::from_config(ctx.config)
            .await
            .map_err(Into::into)
    }
}

/// Builds the [`LocalStore`] at `data_dir/local-store.redb`, creating the
/// data directory if it does not exist.
pub struct LocalStoreBuilder;

impl ComponentBuilder for LocalStoreBuilder {
    type Output = LocalStore;

    async fn build(self, ctx: &BuilderContext<'_>) -> Result<LocalStore, BoxError> {
        // create_dir_all and LocalStore::open (which fsyncs on create) are
        // blocking syscalls; keep them off the async executor.
        let data_dir = ctx.data_dir.to_path_buf();
        ctx.executor
            .spawn_blocking(move || {
                std::fs::create_dir_all(&data_dir).map_err(|e| {
                    BoxError::from(format!("create data directory {}: {e}", data_dir.display()))
                })?;
                let path = data_dir.join("local-store.redb");
                LocalStore::open(&path).map_err(|e| {
                    BoxError::from(format!("open local-store at {}: {e}", path.display()))
                })
            })
            .join()
            .await
            .ok_or_else(|| BoxError::from("local-store open task ended abnormally"))?
    }
}

/// Builds the default [`LogPipeline`]: the byte-bounded in-memory backend
/// sized from `[limits.logs]`.
pub struct LogPipelineBuilder;

impl ComponentBuilder for LogPipelineBuilder {
    type Output = LogPipeline;

    async fn build(self, ctx: &BuilderContext<'_>) -> Result<LogPipeline, BoxError> {
        Ok(LogPipeline::in_memory(ctx.config.limits.logs))
    }
}

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
