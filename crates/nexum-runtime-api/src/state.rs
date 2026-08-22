//! Local-store seam: process-wide store vending per-module namespaced
//! handles.

use crate::BoxError;

/// Cap on ops per [`StateHandle::apply`] batch.
pub const MAX_APPLY_OPS: usize = 1024;

/// Cap on total set-value bytes per [`StateHandle::apply`] batch.
pub const MAX_APPLY_VALUE_BYTES: u64 = 4 * 1024 * 1024;

/// Cap on entries returned by one [`StateHandle::list_entries`] page.
pub const MAX_LIST_LIMIT: u32 = 1024;

/// Cap on entries examined by one [`StateHandle::list_entries`] page.
pub const MAX_LIST_SCAN_LIMIT: u32 = 8192;

/// Cap on key and value bytes carried by one [`StateHandle::list_entries`]
/// page.
pub const MAX_LIST_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Host-side test on a candidate value. Data, not a predicate: the host
/// cannot run guest code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFilter<'a> {
    /// Keep values starting with these bytes.
    HasPrefix(&'a [u8]),
    /// Keep values not starting with these bytes.
    LacksPrefix(&'a [u8]),
}

impl ValueFilter<'_> {
    /// Whether `value` survives the filter.
    #[must_use]
    pub fn keeps(&self, value: &[u8]) -> bool {
        match self {
            Self::HasPrefix(p) => value.starts_with(p),
            Self::LacksPrefix(p) => !value.starts_with(p),
        }
    }
}

/// One [`StateHandle::list_entries`] request.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListQuery<'a> {
    /// Key prefix to scan.
    pub prefix: &'a str,
    /// Exclusive resume key; empty starts at the beginning.
    pub start_after: &'a str,
    /// Entries returned at most; zero takes [`MAX_LIST_LIMIT`].
    pub limit: u32,
    /// Entries examined at most; zero takes [`MAX_LIST_SCAN_LIMIT`].
    pub scan_limit: u32,
    /// Host-side value test; `None` keeps every entry.
    pub filter: Option<ValueFilter<'a>>,
}

/// Resolve an asked-for bound against `cap`: zero takes the cap, past it
/// is `None`.
fn resolve(asked: u32, cap: u32) -> Option<usize> {
    match asked {
        0 => Some(cap as usize),
        n if n <= cap => Some(n as usize),
        _ => None,
    }
}

impl ListQuery<'_> {
    /// Entries this page may return; `None` past [`MAX_LIST_LIMIT`].
    #[must_use]
    pub fn resolved_limit(&self) -> Option<usize> {
        resolve(self.limit, MAX_LIST_LIMIT)
    }

    /// Entries this page may examine; `None` past [`MAX_LIST_SCAN_LIMIT`].
    #[must_use]
    pub fn resolved_scan_limit(&self) -> Option<usize> {
        resolve(self.scan_limit, MAX_LIST_SCAN_LIMIT)
    }
}

/// One page of [`StateHandle::list_entries`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryPage {
    /// Surviving entries in key order.
    pub entries: Vec<(String, Vec<u8>)>,
    /// Last key examined, not the last returned: a filtered page can be
    /// empty mid-scan and must still resume.
    pub last_examined: Option<String>,
    /// Whether the scan reached the end of the prefix range.
    pub exhausted: bool,
}

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
    /// The page asks for more entries than one call may return.
    #[error("list-entries asks for {limit} entries but the cap is {cap}")]
    ListLimitExceeded {
        /// Entries the rejected page asked for.
        limit: u32,
        /// Per-page return cap.
        cap: u32,
    },
    /// The page asks the host to examine more entries than one call may.
    #[error("list-entries would examine {scan_limit} entries but the cap is {cap}")]
    ListScanLimitExceeded {
        /// Entries the rejected page would examine.
        scan_limit: u32,
        /// Per-page scan cap.
        cap: u32,
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
    /// One page of entries under `query`, in key order. The host holds no
    /// cursor between calls, so a caller resumes from
    /// [`EntryPage::last_examined`].
    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, StoreError>;
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
