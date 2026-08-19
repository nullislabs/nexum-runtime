//! [`ComponentsBuilder`] assembles the per-backend [`ComponentBuilder`]s and
//! the log pipeline into a [`Components`] bundle.

use nexum_runtime_api::{BoxError, BuilderContext, ComponentBuilder, RuntimeTypes};
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_logs::{LogPipeline, LogPipelineBuilder};

use crate::component::Components;

/// Names the component slot whose build failed.
#[derive(Debug, thiserror::Error)]
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
