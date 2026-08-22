//! In-memory [`StateStore`] fake: per-namespace `HashMap`, no redb, no disk.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use nexum_runtime_api::{
    EntryPage, ListQuery, MAX_APPLY_OPS, MAX_APPLY_VALUE_BYTES, MAX_LIST_LIMIT,
    MAX_LIST_RESPONSE_BYTES, MAX_LIST_SCAN_LIMIT, StateHandle, StateStore, StoreError, WriteOp,
};

type Namespaces = HashMap<String, HashMap<String, Vec<u8>>>;

/// In-memory store keyed by namespace then key; cheap `Arc` clone shares one
/// backing map, so a test keeps a clone to assert on what a module wrote.
#[derive(Clone, Default)]
pub struct MockStateStore {
    namespaces: Arc<Mutex<Namespaces>>,
}

impl MockStateStore {
    /// Fresh empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Per-module handle over the shared map, scoped to one namespace.
#[derive(Clone)]
pub struct MockStateHandle {
    namespaces: Arc<Mutex<Namespaces>>,
    namespace: String,
    quota_bytes: Option<u64>,
}

impl StateStore for MockStateStore {
    type Handle = MockStateHandle;

    fn module(&self, namespace: &str) -> Result<MockStateHandle, StoreError> {
        // Reject the empty namespace so the handle always has a real prefix,
        // matching the redb-backed store.
        if namespace.is_empty() {
            return Err(StoreError::InvalidNamespace(
                "module namespace must not be empty".into(),
            ));
        }
        Ok(MockStateHandle {
            namespaces: Arc::clone(&self.namespaces),
            namespace: namespace.to_owned(),
            quota_bytes: None,
        })
    }
}

impl MockStateHandle {
    fn lock(&self) -> MutexGuard<'_, Namespaces> {
        self.namespaces.lock()
    }
}

impl StateHandle for MockStateHandle {
    fn with_quota(mut self, quota_bytes: u64) -> Self {
        self.quota_bytes = Some(quota_bytes);
        self
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .lock()
            .get(&self.namespace)
            .and_then(|m| m.get(key))
            .cloned())
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let mut map = self.lock();
        let ns = map.entry(self.namespace.clone()).or_default();
        if let Some(quota) = self.quota_bytes {
            let entry = (key.len() + value.len()) as u64;
            let old = ns
                .get(key)
                .map(|v| (key.len() + v.len()) as u64)
                .unwrap_or(0);
            let used: u64 = ns.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
            let projected = used.saturating_sub(old) + entry;
            if projected > quota {
                return Err(if entry > quota {
                    StoreError::QuotaUnsatisfiable {
                        needed: entry,
                        quota,
                    }
                } else {
                    StoreError::QuotaExceeded {
                        needed: projected,
                        quota,
                    }
                });
            }
        }
        ns.insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        if let Some(m) = self.lock().get_mut(&self.namespace) {
            m.remove(key);
        }
        Ok(())
    }

    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let map = self.lock();
        let mut keys: Vec<String> = map
            .get(&self.namespace)
            .into_iter()
            .flat_map(|m| m.keys())
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        // Sorted for deterministic enumeration, matching the redb B-tree order.
        keys.sort();
        Ok(keys)
    }

    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, StoreError> {
        let limit = query
            .resolved_limit()
            .ok_or(StoreError::ListLimitExceeded {
                limit: query.limit,
                cap: MAX_LIST_LIMIT,
            })?;
        let scan_limit = query
            .resolved_scan_limit()
            .ok_or(StoreError::ListScanLimitExceeded {
                scan_limit: query.scan_limit,
                cap: MAX_LIST_SCAN_LIMIT,
            })?;
        let map = self.lock();
        let mut rows: Vec<(&String, &Vec<u8>)> = map
            .get(&self.namespace)
            .into_iter()
            .flat_map(|m| m.iter())
            .filter(|(k, _)| k.starts_with(query.prefix) && k.as_str() > query.start_after)
            .collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let mut page = EntryPage {
            exhausted: true,
            ..EntryPage::default()
        };
        let mut bytes = 0u64;
        for (examined, (key, value)) in rows.into_iter().enumerate() {
            if examined == scan_limit {
                page.exhausted = false;
                break;
            }
            if query.filter.is_none_or(|f| f.keeps(value)) {
                let cost = (key.len() + value.len()) as u64;
                // The first match always lands, so an oversized value
                // cannot stall the scan at a page that never advances.
                let full = page.entries.len() == limit
                    || (!page.entries.is_empty() && bytes + cost > MAX_LIST_RESPONSE_BYTES);
                if full {
                    page.exhausted = false;
                    break;
                }
                bytes += cost;
                page.entries.push((key.clone(), value.clone()));
            }
            page.last_examined = Some(key.clone());
        }
        Ok(page)
    }

    fn apply(&self, ops: &[WriteOp]) -> Result<(), StoreError> {
        if ops.len() > MAX_APPLY_OPS {
            return Err(StoreError::ApplyOpsExceeded {
                ops: ops.len(),
                cap: MAX_APPLY_OPS,
            });
        }
        let value_bytes: u64 = ops
            .iter()
            .map(|op| match op {
                WriteOp::Set { value, .. } => value.len() as u64,
                WriteOp::Delete { .. } => 0,
            })
            .sum();
        if value_bytes > MAX_APPLY_VALUE_BYTES {
            return Err(StoreError::ApplyBytesExceeded {
                bytes: value_bytes,
                cap: MAX_APPLY_VALUE_BYTES,
            });
        }
        let mut map = self.lock();
        let ns = map.entry(self.namespace.clone()).or_default();
        // Net whole-batch projection, checked once before any mutation so
        // an over-quota batch lands nothing (the map mirrors one txn).
        if let Some(quota) = self.quota_bytes {
            let mut finals: HashMap<&str, Option<usize>> = HashMap::new();
            for op in ops {
                match op {
                    WriteOp::Set { key, value } => finals.insert(key, Some(value.len())),
                    WriteOp::Delete { key } => finals.insert(key, None),
                };
            }
            let used: u64 = ns.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
            let mut released = 0u64;
            let mut charged = 0u64;
            for (key, value_len) in &finals {
                released += ns
                    .get(*key)
                    .map(|v| (key.len() + v.len()) as u64)
                    .unwrap_or(0);
                charged += value_len.map(|len| (key.len() + len) as u64).unwrap_or(0);
            }
            let projected = used.saturating_sub(released) + charged;
            if projected > quota {
                return Err(if charged > quota {
                    StoreError::QuotaUnsatisfiable {
                        needed: charged,
                        quota,
                    }
                } else {
                    StoreError::QuotaExceeded {
                        needed: projected,
                        quota,
                    }
                });
            }
        }
        for op in ops {
            match op {
                WriteOp::Set { key, value } => {
                    ns.insert(key.clone(), value.clone());
                }
                WriteOp::Delete { key } => {
                    ns.remove(key);
                }
            }
        }
        Ok(())
    }
}
