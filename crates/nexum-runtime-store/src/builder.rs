use nexum_runtime_api::{BoxError, BuilderContext, ComponentBuilder};

use crate::local_store_redb::LocalStore;

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
