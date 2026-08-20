//! Chain-log resume cursors, persisted best-effort after a successful
//! dispatch and at each completed bulk-backfill chunk.

use std::collections::{BTreeMap, BTreeSet};

use alloy_chains::Chain;
use alloy_primitives::{Address, B256, keccak256};
use tracing::warn;

use nexum_runtime_api::{StateHandle, StateStore};

/// In-memory cursor mirror, `module -> cursor key -> block`; additions only
/// move forward, retractions pull back to the retracted height.
#[derive(Default)]
pub(super) struct ChainLogCursors {
    cursors: BTreeMap<String, BTreeMap<String, u64>>,
    /// Pairs whose frontier commits are withheld after a failed dispatch.
    holds: BTreeSet<(String, String)>,
}

impl ChainLogCursors {
    /// Cursor value to persist, or `None` when unchanged; `seed` runs only
    /// the first time the pair is seen.
    pub(super) fn record(
        &mut self,
        module: &str,
        key: &str,
        block: u64,
        removed: bool,
        seed: impl FnOnce() -> Option<u64>,
    ) -> Option<u64> {
        let tracked = self
            .cursors
            .get(module)
            .and_then(|keys| keys.get(key))
            .copied();
        let current = match tracked {
            Some(c) => Some(c),
            None => seed(),
        };
        let next = match current {
            Some(c) if removed => c.min(block),
            Some(c) => c.max(block),
            None => block,
        };
        if tracked != Some(next) {
            self.cursors
                .entry(module.to_owned())
                .or_default()
                .insert(key.to_owned(), next);
        }
        (current != Some(next)).then_some(next)
    }

    /// Withhold `(module, key)`'s frontier commits for the rest of the
    /// process, so a restart replays what a failed dispatch missed.
    pub(super) fn hold(&mut self, module: &str, key: &str) {
        self.holds.insert((module.to_owned(), key.to_owned()));
    }

    fn is_held(&self, module: &str, key: &str) -> bool {
        self.holds.contains(&(module.to_owned(), key.to_owned()))
    }
}

/// Host-owned store namespace for `module`'s cursor. A module name cannot
/// contain `/`, so no author-supplied name can produce this namespace.
pub(super) fn host_namespace(module: &str) -> String {
    format!("host/{module}")
}

/// Persisted cursor for `(module, key)`; `None` when absent or unreadable.
/// Decodes the little-endian pair of [`commit_chain_log_cursor`]'s encode.
pub(super) fn read_chain_log_cursor<S: StateStore>(
    store: &S,
    module: &str,
    key: &str,
) -> Option<u64> {
    let handle = store.module(&host_namespace(module)).ok()?;
    let bytes = handle.get(key).ok()??;
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

/// Persist the cursor; writes only when [`ChainLogCursors::record`] moves.
pub(super) fn commit_chain_log_cursor<S: StateStore>(
    store: &S,
    cursors: &mut ChainLogCursors,
    module: &str,
    key: &str,
    block: u64,
    removed: bool,
) {
    let Some(cursor) = cursors.record(module, key, block, removed, || {
        read_chain_log_cursor(store, module, key)
    }) else {
        return;
    };
    match store.module(&host_namespace(module)) {
        Ok(ms) => {
            if let Err(e) = ms.set(key, &cursor.to_le_bytes()) {
                warn!(
                    module = %module,
                    error = %e,
                    "failed to persist event source cursor",
                );
            }
        }
        Err(e) => warn!(
            module = %module,
            error = %e,
            "failed to open host store for event source cursor",
        ),
    }
}

/// Persist a completed bulk chunk's frontier as the cursor; a held pair
/// commits nothing.
pub(super) fn commit_chain_log_frontier<S: StateStore>(
    store: &S,
    cursors: &mut ChainLogCursors,
    module: &str,
    key: &str,
    frontier: u64,
) {
    if cursors.is_held(module, key) {
        return;
    }
    commit_chain_log_cursor(store, cursors, module, key, frontier, false);
}

/// Keyed on `0x`-prefixed lowercase hex, not the alloy `Filter` (whose hash
/// is process-randomized), so it is stable across a restart and across the
/// typing of the manifest values it was formerly derived from.
pub(super) fn chainlog_cursor_key(
    chain: Chain,
    address: Option<Address>,
    event_signature: Option<B256>,
) -> String {
    let normalized = format!(
        "{}|{}|{}",
        chain.id(),
        address.map(|a| format!("{a:#x}")).unwrap_or_default(),
        event_signature
            .map(|t| format!("{t:#x}"))
            .unwrap_or_default(),
    );
    format!("chainlog_cursor:{:x}", keccak256(normalized.as_bytes()))
}
