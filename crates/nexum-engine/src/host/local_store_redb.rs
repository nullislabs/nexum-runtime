//! `nexum:host/local-store` backend.
//!
//! Single redb file under `EngineConfig.engine.state_dir`. Per-module
//! namespacing is enforced host-side via a fixed-length 32-byte prefix:
//! `keccak256(module_name) ++ raw_key`. Two modules using the same key
//! string see disjoint data regardless of how similar their names are.
//!
//! The 32-byte hash prefix has two properties that the old
//! `[len:u8][name][key]` scheme lacked:
//!
//! - **Fixed width** - no length field to forge; a module cannot craft a
//!   key that bleeds into another module's prefix range.
//! - **ENS-compatible** - keccak256 is the same hash used by ENS node
//!   derivation, so module identities can be derived from ENS names
//!   without extra hashing in the future (ADR-0003).

#![allow(clippy::result_large_err)]

use std::path::Path;
use std::sync::Arc;

use alloy_primitives::keccak256;
use redb::{Database, TableDefinition};
use thiserror::Error;

const TABLE: TableDefinition<'static, &[u8], &[u8]> = TableDefinition::new("nexum:local-store");
#[cfg(test)]
const PREFIX_LEN: usize = 32;

/// Process-wide handle to the local-store redb database. Cheap to
/// clone; the per-module view is constructed by setting the namespace
/// prefix at call time.
#[derive(Debug, Clone)]
pub struct LocalStore {
    db: Arc<Database>,
}

impl LocalStore {
    /// Open (or create) the redb file at `path`. Materialises the shared
    /// table so subsequent read transactions never hit `TableDoesNotExist`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let db = Database::create(path).map_err(StorageError::Open)?;
        {
            let txn = db.begin_write().map_err(StorageError::Txn)?;
            txn.open_table(TABLE).map_err(StorageError::Table)?;
            txn.commit().map_err(StorageError::Commit)?;
        }
        Ok(Self { db: Arc::new(db) })
    }

    /// Fetch a value for `(namespace, key)`. Returns `Ok(None)` when
    /// no entry exists; the module never observes the prefix.
    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let full = build_key(namespace, key)?;
        let txn = self.db.begin_read().map_err(StorageError::Txn)?;
        let table = txn.open_table(TABLE).map_err(StorageError::Table)?;
        let value = table
            .get(full.as_slice())
            .map_err(StorageError::Storage)?
            .map(|v| v.value().to_vec());
        Ok(value)
    }

    /// Insert or overwrite.
    pub fn set(&self, namespace: &str, key: &str, value: &[u8]) -> Result<(), StorageError> {
        let full = build_key(namespace, key)?;
        let txn = self.db.begin_write().map_err(StorageError::Txn)?;
        {
            let mut table = txn.open_table(TABLE).map_err(StorageError::Table)?;
            table
                .insert(full.as_slice(), value)
                .map_err(StorageError::Storage)?;
        }
        txn.commit().map_err(StorageError::Commit)?;
        Ok(())
    }

    /// Delete. Idempotent - deleting a missing key is a no-op.
    pub fn delete(&self, namespace: &str, key: &str) -> Result<(), StorageError> {
        let full = build_key(namespace, key)?;
        let txn = self.db.begin_write().map_err(StorageError::Txn)?;
        {
            let mut table = txn.open_table(TABLE).map_err(StorageError::Table)?;
            table
                .remove(full.as_slice())
                .map_err(StorageError::Storage)?;
        }
        txn.commit().map_err(StorageError::Commit)?;
        Ok(())
    }

    /// Enumerate keys in `namespace` whose raw key (post-prefix) starts
    /// with `prefix`. Returns only the module-visible key strings - the
    /// host strips the namespace prefix.
    pub fn list_keys(&self, namespace: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let ns_prefix = namespace_prefix(namespace)?;
        let full_prefix = build_key(namespace, prefix)?;
        let txn = self.db.begin_read().map_err(StorageError::Txn)?;
        let table = txn.open_table(TABLE).map_err(StorageError::Table)?;
        let mut out = Vec::new();
        // redb's B-tree iterates keys in sorted order, so a range
        // starting at `full_prefix` only touches matching entries (and
        // the first key past the prefix range). Breaking on the first
        // non-matching key keeps this O(matching entries) instead of
        // the O(total DB entries) `table.iter()` would do.
        for entry in table
            .range(full_prefix.as_slice()..)
            .map_err(StorageError::Storage)?
        {
            let (k, _v) = entry.map_err(StorageError::Storage)?;
            let key_bytes = k.value();
            if !key_bytes.starts_with(&full_prefix) {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&key_bytes[ns_prefix.len()..]) {
                out.push(s.to_owned());
            }
        }
        Ok(out)
    }
}

/// Returns the 32-byte keccak256 hash of `namespace` as a `Vec<u8>`.
/// Rejects the empty string so callers can rely on a non-trivial prefix.
fn namespace_prefix(namespace: &str) -> Result<Vec<u8>, StorageError> {
    if namespace.is_empty() {
        return Err(StorageError::InvalidNamespace(
            "module namespace must not be empty".into(),
        ));
    }
    Ok(keccak256(namespace.as_bytes()).to_vec())
}

fn build_key(namespace: &str, key: &str) -> Result<Vec<u8>, StorageError> {
    let mut out = namespace_prefix(namespace)?;
    out.extend_from_slice(key.as_bytes());
    Ok(out)
}

/// Errors surfaced by [`LocalStore`].
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("open redb: {0}")]
    Open(#[source] redb::DatabaseError),
    #[error("redb txn: {0}")]
    Txn(#[source] redb::TransactionError),
    #[error("redb table: {0}")]
    Table(#[source] redb::TableError),
    #[error("redb storage: {0}")]
    Storage(#[source] redb::StorageError),
    #[error("redb commit: {0}")]
    Commit(#[source] redb::CommitError),
    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),
}

#[cfg(test)]
mod tests;
