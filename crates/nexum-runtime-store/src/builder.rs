use std::path::PathBuf;

use nexum_runtime_api::{BoxError, BuilderContext, ComponentBuilder};
use tracing::info;

use crate::local_store_redb::{LocalStore, StorageError};

/// Builds the [`LocalStore`] at `data_dir/local-store.redb`, creating the
/// data directory if it does not exist.
pub struct LocalStoreBuilder;

/// The local store did not open. The typed cause stays in the chain, so
/// the boot path can tell a format refusal from an I/O failure.
#[derive(Debug, thiserror::Error)]
#[error("open local-store at {}: {source}", path.display())]
pub struct StoreOpenError {
    /// The database file.
    pub path: PathBuf,
    /// Why it did not open.
    #[source]
    pub source: StorageError,
}

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
                let store = match LocalStore::open(&path) {
                    Ok(store) => store,
                    Err(source) => return Err(StoreOpenError { path, source }.into()),
                };
                info!(
                    path = %path.display(),
                    format_version = store.format_version(),
                    found = ?store.format_found(),
                    "local store open"
                );
                Ok(store)
            })
            .join()
            .await
            .ok_or_else(|| BoxError::from("local-store open task ended abnormally"))?
    }
}
