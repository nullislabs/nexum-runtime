//! The redb local-store backend for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
mod local_store_redb;

pub use builder::LocalStoreBuilder;
pub use local_store_redb::{LocalStore, ModuleStore, StorageError};
