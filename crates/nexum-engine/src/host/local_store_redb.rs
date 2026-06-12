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
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, LocalStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LocalStore::open(dir.path().join("ls.redb")).expect("open");
        (dir, store)
    }

    #[test]
    fn set_get_roundtrip() {
        let (_dir, store) = fresh();
        store.set("twap", "k", b"v").unwrap();
        assert_eq!(store.get("twap", "k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn namespaces_isolate_modules() {
        let (_dir, store) = fresh();
        store.set("a", "k", b"from-a").unwrap();
        store.set("b", "k", b"from-b").unwrap();
        assert_eq!(
            store.get("a", "k").unwrap().as_deref(),
            Some(&b"from-a"[..])
        );
        assert_eq!(
            store.get("b", "k").unwrap().as_deref(),
            Some(&b"from-b"[..])
        );
    }

    #[test]
    fn delete_then_get_is_none() {
        let (_dir, store) = fresh();
        store.set("twap", "k", b"v").unwrap();
        store.delete("twap", "k").unwrap();
        assert!(store.get("twap", "k").unwrap().is_none());
    }

    #[test]
    fn list_keys_strips_namespace_prefix() {
        let (_dir, store) = fresh();
        store.set("twap", "posted:1", b"x").unwrap();
        store.set("twap", "posted:2", b"y").unwrap();
        store.set("twap", "other", b"z").unwrap();
        let keys = store.list_keys("twap", "posted:").unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| k.starts_with("posted:")));
    }

    #[test]
    fn rejects_empty_namespace() {
        let (_dir, store) = fresh();
        let err = store.set("", "k", b"v").unwrap_err();
        assert!(matches!(err, StorageError::InvalidNamespace(_)));
    }

    #[test]
    fn prefix_is_fixed_32_bytes() {
        let short = namespace_prefix("a").unwrap();
        let long = namespace_prefix(&"a".repeat(300)).unwrap();
        assert_eq!(short.len(), PREFIX_LEN);
        assert_eq!(long.len(), PREFIX_LEN);
        // Different inputs produce different prefixes.
        assert_ne!(short, long);
    }

    #[test]
    fn prefix_is_deterministic() {
        let p1 = namespace_prefix("twap-monitor").unwrap();
        let p2 = namespace_prefix("twap-monitor").unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn similar_names_differ() {
        // Verify that names that share a common prefix don't collide.
        let pa = namespace_prefix("module-a").unwrap();
        let pb = namespace_prefix("module-b").unwrap();
        assert_ne!(pa, pb);
    }
}
