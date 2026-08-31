//! Project loaded modules' triggers into the sources the host opens;
//! dead modules are excluded so no stream opens for an unreachable module.

use std::collections::BTreeSet;

use alloy_chains::Chain;

use super::Supervisor;
use super::cursors::{chainlog_cursor_key, read_chain_log_cursor};
use crate::bindings::nexum;
use crate::manifest::Trigger;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::RuntimeTypes;

impl<T: RuntimeTypes> Supervisor<T> {
    /// One pass, one health filter: a dead module contributes to no field,
    /// so no stream of any kind opens for it.
    pub fn source_plan(&self) -> SourcePlan {
        let mut block_chains: Vec<Chain> = Vec::new();
        let mut event_sources = Vec::new();
        let mut demanded_extension_kinds = BTreeSet::new();
        let mut dead_hold_triggers = false;
        for module in &self.modules {
            if !module.health.dispatchable() {
                dead_hold_triggers |= !module.triggers.is_empty();
                continue;
            }
            for trigger in &module.triggers {
                match trigger {
                    Trigger::Block { chain_id } => {
                        block_chains.push(Chain::from_id(*chain_id));
                    }
                    Trigger::Event {
                        chain_id,
                        address,
                        event_signature,
                        resume,
                        max_lookback,
                        start_block,
                    } => {
                        let filter = build_alloy_filter(*address, *event_signature);
                        let chain = Chain::from_id(*chain_id);
                        // A `resume` trigger reads its durable cursor
                        // once here at boot; others start at head.
                        // `start_block` seeds only the first boot, when
                        // no cursor is stored yet: a module whose whole
                        // state derives from logs cannot start at head,
                        // because history it never saw is history it can
                        // never rebuild. The stored cursor wins after
                        // that, so the seed is not a floor and does not
                        // re-apply on restart. The manifest rejects
                        // `start_block` without `resume`, which would
                        // otherwise rescan from it on every open.
                        let (cursor_key, initial_cursor) = if *resume {
                            let key = chainlog_cursor_key(chain, *address, *event_signature);
                            let seed = read_chain_log_cursor(
                                &self.shared.components.store,
                                module.name.as_str(),
                                &key,
                            )
                            .or(*start_block);
                            (Some(key), seed)
                        } else {
                            (None, None)
                        };
                        event_sources.push(EventSource {
                            module: module.name.clone(),
                            chain,
                            filter,
                            cursor_key,
                            initial_cursor,
                            max_lookback: *max_lookback,
                        });
                    }
                    Trigger::Extension { extension_kind, .. } => {
                        demanded_extension_kinds.insert(extension_kind.clone());
                    }
                    Trigger::Schedule { .. } => {}
                }
            }
        }
        block_chains.sort_by_key(|c| c.id());
        block_chains.dedup();
        SourcePlan {
            block_chains,
            event_sources,
            demanded_extension_kinds,
            dead_hold_triggers,
        }
    }
}

/// Everything the launch path opens, projected once from the live modules.
pub struct SourcePlan {
    /// Sorted by numeric id and deduped.
    pub block_chains: Vec<Chain>,
    /// The stream tags every log with the owning module for routing.
    pub event_sources: Vec<EventSource>,
    /// An extension opens a source only for kinds appearing here.
    pub demanded_extension_kinds: BTreeSet<String>,
    /// A dead module declares at least one trigger.
    pub dead_hold_triggers: bool,
}

impl SourcePlan {
    /// A declared extension kind is not yet a source: the extension gates on
    /// its own service state, so the caller passes how many really opened.
    pub fn viability(&self, open_extension_sources: usize) -> Viability {
        if !self.block_chains.is_empty()
            || !self.event_sources.is_empty()
            || open_extension_sources > 0
        {
            Viability::Live
        } else if self.dead_hold_triggers {
            Viability::DeadHoldTriggers
        } else {
            Viability::Nothing
        }
    }
}

/// The launch verdict; boot-dead is permanent, so it is final at launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viability {
    /// No module declares a trigger; the engine has nothing to run.
    Nothing,
    /// Every declared trigger belongs to a dead module.
    DeadHoldTriggers,
    /// At least one open source drives the engine.
    Live,
}

/// One chain-log source to open, resolved from a module's event trigger.
pub struct EventSource {
    /// Module whose trigger opened this source.
    pub module: ModuleId,
    /// Chain the filter runs against; it must have an `engine.toml` entry.
    pub chain: Chain,
    /// Address and topic filter, built from the manifest.
    pub filter: alloy_rpc_types_eth::Filter,
    /// `Some` iff `resume = true`: the key the resume cursor lives under.
    pub cursor_key: Option<String>,
    /// Read once at boot; `None` unless `resume = true`.
    pub initial_cursor: Option<u64>,
    /// Opt-in cap on backfill depth, in blocks. `None` backfills the whole
    /// gap; `Some(cap)` bounds the start to `head - cap`, undershot by up to
    /// the revalidation depth while a tail retraction is pending.
    pub max_lookback: Option<u64>,
}

/// The chain id is not on the alloy log; the batch level supplies it.
pub(super) fn wit_log(log: &alloy_rpc_types_eth::Log, chain: Chain) -> nexum::host::types::Log {
    nexum::host::types::Log {
        chain_id: chain.id(),
        address: log.address().as_slice().to_vec(),
        topics: log.topics().iter().map(|t| t.as_slice().to_vec()).collect(),
        data: log.inner.data.data.to_vec(),
        block_hash: log.block_hash.map(|h| h.as_slice().to_vec()),
        block_number: log.block_number,
        block_timestamp: log.block_timestamp,
        transaction_hash: log.transaction_hash.map(|h| h.as_slice().to_vec()),
        transaction_index: log.transaction_index,
        log_index: log.log_index,
        removed: log.removed,
    }
}

/// Infallible: the manifest carries typed filter values.
pub(super) fn build_alloy_filter(
    address: Option<alloy_primitives::Address>,
    event_signature: Option<alloy_primitives::B256>,
) -> alloy_rpc_types_eth::Filter {
    let mut filter = alloy_rpc_types_eth::Filter::new();
    if let Some(addr) = address {
        filter = filter.address(addr);
    }
    if let Some(topic) = event_signature {
        filter = filter.event_signature(topic);
    }
    filter
}
