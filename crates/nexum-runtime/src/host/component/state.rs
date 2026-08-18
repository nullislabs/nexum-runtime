//! Local-store seam: process-wide store vending per-module namespaced
//! handles.

use crate::error::BoxError;

/// One write in a [`StateHandle::apply`] batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Insert or overwrite `key` with `value`.
    Set {
        /// Module-visible key.
        key: String,
        /// Value bytes.
        value: Vec<u8>,
    },
    /// Delete `key`; a missing key is a no-op.
    Delete {
        /// Module-visible key.
        key: String,
    },
}

/// A refusal or failure from a [`StateStore`] or [`StateHandle`] call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The namespace cannot name a store partition.
    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),
    /// The write would push the module past its byte quota.
    #[error("local-store quota exceeded: write needs {needed} B but quota is {quota} B")]
    QuotaExceeded {
        /// Footprint the write would produce.
        needed: u64,
        /// The module's byte quota.
        quota: u64,
    },
    /// The write alone exceeds the whole quota.
    #[error("local-store write needs {needed} B alone but the quota is {quota} B")]
    QuotaUnsatisfiable {
        /// On-disk cost of just this write's entries.
        needed: u64,
        /// The module's byte quota.
        quota: u64,
    },
    /// The batch declares more operations than one `apply` may carry.
    #[error("apply batch has {ops} ops but the cap is {cap}")]
    ApplyOpsExceeded {
        /// Ops in the rejected batch.
        ops: usize,
        /// Per-batch op cap.
        cap: usize,
    },
    /// The batch's set values exceed the per-batch byte cap.
    #[error("apply batch carries {bytes} value B but the cap is {cap} B")]
    ApplyBytesExceeded {
        /// Total set-value bytes in the rejected batch.
        bytes: u64,
        /// Per-batch value-byte cap.
        cap: u64,
    },
    /// The storage backend failed.
    #[error("store backend: {0}")]
    Backend(#[source] BoxError),
}

/// Process-wide state store that vends per-module handles.
pub trait StateStore {
    /// Per-module namespaced handle type.
    type Handle: StateHandle;

    /// Return a handle scoped to `namespace`.
    fn module(&self, namespace: &str) -> Result<Self::Handle, StoreError>;
}

/// Per-module key-value handle.
pub trait StateHandle {
    /// Cap this handle at `quota_bytes` (key + value bytes); writes past it
    /// are rejected with [`StoreError::QuotaExceeded`], or
    /// [`StoreError::QuotaUnsatisfiable`] when the write alone cannot fit.
    #[must_use]
    fn with_quota(self, quota_bytes: u64) -> Self;
    /// Fetch a value; `Ok(None)` when absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// Insert or overwrite.
    fn set(&self, key: &str, value: &[u8]) -> Result<(), StoreError>;
    /// Delete; idempotent.
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// Enumerate module-visible keys starting with `prefix`.
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
    /// Whether `key` exists.
    fn contains(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.get(key)?.is_some())
    }
    /// Value byte length, `Ok(None)` when absent.
    fn len(&self, key: &str) -> Result<Option<u64>, StoreError> {
        Ok(self.get(key)?.map(|v| v.len() as u64))
    }
    /// Number of keys starting with `prefix`.
    fn count(&self, prefix: &str) -> Result<u64, StoreError> {
        Ok(self.list_keys(prefix)?.len() as u64)
    }
    /// Apply `ops` as one atomic batch; caps op count and total value bytes.
    fn apply(&self, ops: &[WriteOp]) -> Result<(), StoreError>;
}
