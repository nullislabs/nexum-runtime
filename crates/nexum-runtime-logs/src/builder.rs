use nexum_runtime_api::{BoxError, BuilderContext, ComponentBuilder};

use crate::logs::LogPipeline;

/// Builds the default [`LogPipeline`]: the byte-bounded in-memory backend
/// sized from `[limits.logs]`.
pub struct LogPipelineBuilder;

impl ComponentBuilder for LogPipelineBuilder {
    type Output = LogPipeline;

    async fn build(self, ctx: &BuilderContext<'_>) -> Result<LogPipeline, BoxError> {
        Ok(LogPipeline::in_memory(ctx.config.limits.logs))
    }
}
