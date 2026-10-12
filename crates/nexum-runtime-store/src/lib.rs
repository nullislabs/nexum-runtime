//! The redb local-store backend for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
mod format;
mod local_store_redb;

pub use builder::{LocalStoreBuilder, StoreOpenError};
pub use format::{FORMAT_VERSION, FormatFound};
pub use local_store_redb::{LocalStore, ModuleStore, StorageError};
