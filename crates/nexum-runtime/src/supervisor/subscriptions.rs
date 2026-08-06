//! Project loaded modules' subscriptions into what the event loop opens;
//! dead modules are excluded so no stream opens for an unreachable module.

use std::collections::BTreeSet;

use alloy_chains::Chain;

use super::Supervisor;
use super::cursors::{chainlog_cursor_key, read_chain_log_cursor};
use crate::bindings::nexum;
use crate::host::component::RuntimeTypes;
use crate::manifest::Subscription;
use crate::module_id::ModuleId;

impl<T: RuntimeTypes> Supervisor<T> {
    /// One pass, one health filter: a dead module contributes to no field,
    /// so no stream of any kind opens for it.
    pub fn subscription_plan(&self) -> SubscriptionPlan {
        let mut block_chains: Vec<Chain> = Vec::new();
        let mut chain_log_subs = Vec::new();
        let mut extension_kinds = BTreeSet::new();
        let mut dead_subscribers = false;
        for module in &self.modules {
            if !module.health.dispatchable() {
                dead_subscribers |= !module.subscriptions.is_empty();
                continue;
            }
            for sub in &module.subscriptions {
                match sub {
                    Subscription::Block { chain_id } => {
                        block_chains.push(Chain::from_id(*chain_id));
                    }
                    Subscription::ChainLog {
                        chain_id,
                        address,
                        event_signature,
                        resume,
                        max_lookback,
                    } => {
                        let filter =
                            build_alloy_filter(address.as_deref(), event_signature.as_deref())
                                .expect("chain-log filters are validated at load");
                        let chain = Chain::from_id(*chain_id);
                        // A `resume` subscription reads its durable cursor
                        // once here at boot; others start at head.
                        let (cursor_key, initial_cursor) = if *resume {
                            let key = chainlog_cursor_key(
                                chain,
                                address.as_deref(),
                                event_signature.as_deref(),
                            );
                            let seed = read_chain_log_cursor(
                                &self.shared.components.store,
                                module.name.as_str(),
                                &key,
                            );
                            (Some(key), seed)
                        } else {
                            (None, None)
                        };
                        chain_log_subs.push(ChainLogSub {
                            module: module.name.clone(),
                            chain,
                            filter,
                            cursor_key,
                            initial_cursor,
                            max_lookback: *max_lookback,
                        });
                    }
                    Subscription::Extension { kind, .. } => {
                        extension_kinds.insert(kind.clone());
                    }
                    Subscription::Cron { .. } => {}
                }
            }
        }
        block_chains.sort_by_key(|c| c.id());
        block_chains.dedup();
        SubscriptionPlan {
            block_chains,
            chain_log_subs,
            extension_kinds,
            dead_subscribers,
        }
    }
}

/// Everything the launch path opens, projected once from the live modules.
pub struct SubscriptionPlan {
    /// Sorted by numeric id and deduped.
    pub block_chains: Vec<Chain>,
    /// The stream tags every log with the owning module for routing.
    pub chain_log_subs: Vec<ChainLogSub>,
    /// An extension opens an event source only for kinds appearing here.
    pub extension_kinds: BTreeSet<String>,
    /// A dead module declares at least one subscription.
    pub dead_subscribers: bool,
}

impl SubscriptionPlan {
    /// A declared extension kind is not yet a source: the extension gates on
    /// its own service state, so the caller passes how many really opened.
    pub fn viability(&self, open_extension_sources: usize) -> Viability {
        if !self.block_chains.is_empty()
            || !self.chain_log_subs.is_empty()
            || open_extension_sources > 0
        {
            Viability::Live
        } else if self.dead_subscribers {
            Viability::DeadHoldSubs
        } else {
            Viability::Nothing
        }
    }
}

/// The launch verdict; boot-dead is permanent, so it is final at launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viability {
    /// No module declares a subscription; the engine has nothing to run.
    Nothing,
    /// Every declared subscription belongs to a dead module.
    DeadHoldSubs,
    /// At least one event source drives the engine.
    Live,
}

pub struct ChainLogSub {
    /// Also the module's store namespace.
    pub module: ModuleId,
    pub chain: Chain,
    pub filter: alloy_rpc_types_eth::Filter,
    /// `Some` iff `resume = true`: the key the resume cursor lives under.
    pub cursor_key: Option<String>,
    /// Read once at boot; `None` unless `resume = true`.
    pub initial_cursor: Option<u64>,
    /// Opt-in cap on backfill depth, in blocks. `None` backfills the whole
    /// gap; `Some(cap)` bounds the start to `head - cap`.
    pub max_lookback: Option<u64>,
}

impl From<&alloy_rpc_types_eth::Log> for nexum::host::types::ChainLog {
    /// The chain id is not on the alloy log; the batch level supplies it.
    fn from(log: &alloy_rpc_types_eth::Log) -> Self {
        Self {
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
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(super) enum FilterError {
    /// `[[subscriptions]].address` did not parse as an EVM address.
    #[error("invalid chain-log address {address:?}: {source}")]
    Address {
        address: String,
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// `[[subscriptions]].event_signature` did not parse as a 32-byte topic.
    #[error("invalid topic {topic:?}: {source}")]
    Topic {
        topic: String,
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
}

pub(super) fn build_alloy_filter(
    address: Option<&str>,
    event_signature: Option<&str>,
) -> std::result::Result<alloy_rpc_types_eth::Filter, FilterError> {
    use alloy_primitives::{Address, B256};
    let mut filter = alloy_rpc_types_eth::Filter::new();
    if let Some(addr_hex) = address {
        let addr: Address = addr_hex.parse().map_err(|source| FilterError::Address {
            address: addr_hex.to_owned(),
            source,
        })?;
        filter = filter.address(addr);
    }
    if let Some(topic_hex) = event_signature {
        let topic: B256 = topic_hex.parse().map_err(|source| FilterError::Topic {
            topic: topic_hex.to_owned(),
            source,
        })?;
        filter = filter.event_signature(topic);
    }
    Ok(filter)
}
