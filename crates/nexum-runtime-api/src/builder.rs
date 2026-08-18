//! The per-component builder seam.

use std::future::Future;
use std::path::Path;

use nexum_runtime_config::EngineConfig;
use nexum_tasks::TaskExecutor;

use crate::BoxError;

/// Shared inputs every component builder reads.
pub struct BuilderContext<'a> {
    /// The loaded engine config.
    pub config: &'a EngineConfig,
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
