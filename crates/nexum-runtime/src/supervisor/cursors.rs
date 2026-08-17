//! Durable dispatch progress: progress markers and chain-log resume cursors;
//! writes are best-effort and happen only after a successful dispatch.

use std::collections::BTreeMap;

use alloy_chains::Chain;
use alloy_primitives::{Address, B256, keccak256};
use tracing::warn;

use crate::host::component::{StateHandle, StateStore};

/// In-memory cursor mirror, `module -> cursor key -> block`; additions only
/// move forward, retractions pull back to the retracted height.
#[derive(Default)]
pub(super) struct ChainLogCursors(BTreeMap<String, BTreeMap<String, u64>>);

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
        let tracked = self.0.get(module).and_then(|keys| keys.get(key)).copied();
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
            self.0
                .entry(module.to_owned())
                .or_default()
                .insert(key.to_owned(), next);
        }
        (current != Some(next)).then_some(next)
    }
}

/// Persisted cursor for `(module, key)`; `None` when absent or unreadable.
/// Decodes the little-endian pair of [`commit_chain_log_cursor`]'s encode.
pub(super) fn read_chain_log_cursor<S: StateStore>(
    store: &S,
    module: &str,
    key: &str,
) -> Option<u64> {
    let handle = store.module(module).ok()?;
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
    match store.module(module) {
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
            "failed to open module store for event source cursor",
        ),
    }
}

/// Persisted per-chain progress key; must stay numeric for data compat.
pub(super) fn progress_key(chain: Chain) -> String {
    format!("last_dispatched_block:{}", chain.id())
}

/// Written only after a successful block dispatch; a failed write warns and
/// dispatch continues.
pub(super) fn persist_progress_marker<S: StateStore>(
    store: &S,
    module: &str,
    chain: Chain,
    block_number: u64,
) {
    let chain_id = chain.id();
    let key = progress_key(chain);
    match store.module(module) {
        Ok(ms) => {
            if let Err(e) = ms.set(&key, &block_number.to_le_bytes()) {
                warn!(
                    module = %module,
                    chain_id,
                    error = %e,
                    "failed to persist last_dispatched_block marker",
                );
            }
        }
        Err(e) => {
            warn!(
                module = %module,
                chain_id,
                error = %e,
                "failed to open module store for progress marker",
            );
        }
    }
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
