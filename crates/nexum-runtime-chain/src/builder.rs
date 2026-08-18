use nexum_runtime_api::{BoxError, BuilderContext, ComponentBuilder};

use crate::provider_pool::ProviderPool;

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
