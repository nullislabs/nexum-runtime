//! Project loaded modules' subscriptions into what the event loop opens:
//! block chains, resolved chain-log filters with resume cursors, and the
//! extension kind set. Dead modules are excluded so no live subscription
//! opens for an unreachable module.

use std::collections::BTreeSet;

use alloy_chains::Chain;
use tracing::warn;

use super::Supervisor;
use super::cursors::{chainlog_cursor_key, read_chain_log_cursor};
use crate::bindings::nexum;
use crate::host::component::RuntimeTypes;
use crate::manifest::Subscription;

impl<T: RuntimeTypes> Supervisor<T> {
    /// Chains any alive module subscribes to block events on, sorted by
    /// numeric id and deduped.
    pub fn block_chains(&self) -> Vec<Chain> {
        let mut out: Vec<Chain> = Vec::new();
        for module in self.modules.iter().filter(|m| m.alive) {
            for sub in &module.subscriptions {
                if let Subscription::Block { chain_id } = sub {
                    out.push(Chain::from_id(*chain_id));
                }
            }
        }
        out.sort_by_key(|c| c.id());
        out.dedup();
        out
    }

    /// Per-module chain-log subscriptions for alive modules only. Each
    /// entry names the module, chain, and filter the event loop opens; the
    /// stream tags every log with `module_name` for routing.
    pub fn chain_log_subscriptions(&self) -> Vec<ChainLogSub> {
        let mut out = Vec::new();
        for module in self.modules.iter().filter(|m| m.alive) {
            for sub in &module.subscriptions {
                if let Subscription::ChainLog {
                    chain_id,
                    address,
                    event_signature,
                    resume,
                    max_lookback,
                } = sub
                {
                    match build_alloy_filter(address.as_deref(), event_signature.as_deref()) {
                        Ok(filter) => {
                            let chain = Chain::from_id(*chain_id);
                            // A `resume` subscription gets a durable cursor
                            // key and its persisted resume point, read once
                            // here at boot; others start at head as before.
                            let (cursor_key, initial_cursor) = if *resume {
                                let key = chainlog_cursor_key(
                                    chain,
                                    address.as_deref(),
                                    event_signature.as_deref(),
                                );
                                let seed = read_chain_log_cursor(
                                    &self.shared.components.store,
                                    &module.name,
                                    &key,
                                );
                                (Some(key), seed)
                            } else {
                                (None, None)
                            };
                            out.push(ChainLogSub {
                                module: module.name.clone(),
                                chain,
                                filter,
                                cursor_key,
                                initial_cursor,
                                max_lookback: *max_lookback,
                            });
                        }
                        Err(err) => warn!(
                            module = %module.name,
                            chain_id,
                            error = %err,
                            "invalid chain-log subscription - skipping",
                        ),
                    }
                }
            }
        }
        out
    }

    /// Extension subscription kinds at least one loaded module declares. An
    /// extension opens an event source only when its kind appears here.
    pub fn extension_subscription_kinds(&self) -> BTreeSet<String> {
        self.modules
            .iter()
            .flat_map(|m| m.subscriptions.iter())
            .filter_map(|s| match s {
                Subscription::Extension { kind, .. } => Some(kind.clone()),
                _ => None,
            })
            .collect()
    }
}

/// A resolved chain-log subscription for the event loop: owning module,
/// chain, alloy `Filter`, and, when `resume` is set, the durable cursor key
/// and resume block.
pub struct ChainLogSub {
    /// Module that declared the subscription; also its store namespace.
    pub module: String,
    /// Chain the filter applies to.
    pub chain: Chain,
    /// Alloy filter the poller opens with.
    pub filter: alloy_rpc_types_eth::Filter,
    /// `Some` iff `resume = true`: the store key the resume cursor lives
    /// under.
    pub cursor_key: Option<String>,
    /// The persisted resume block, read at boot for a `resume`
    /// subscription; `None` otherwise.
    pub initial_cursor: Option<u64>,
    /// Opt-in cap on backfill depth, in blocks. `None` backfills the whole
    /// gap; `Some(cap)` bounds the start to `head - cap`.
    pub max_lookback: Option<u64>,
}

impl From<&alloy_rpc_types_eth::Log> for nexum::host::types::ChainLog {
    /// Project an alloy `Log` onto the WIT `chain-log` record without loss.
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

/// Errors surfaced by [`build_alloy_filter`].
#[derive(Debug, thiserror::Error, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub(super) enum FilterError {
    /// `[[subscriptions]].address` did not parse as an EVM address.
    #[error("invalid chain-log address {address:?}: {source}")]
    Address {
        /// Raw operator-supplied hex string.
        address: String,
        /// Underlying alloy parse failure.
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// `[[subscriptions]].event_signature` did not parse as a 32-byte topic.
    #[error("invalid topic {topic:?}: {source}")]
    Topic {
        /// Raw operator-supplied hex string.
        topic: String,
        /// Underlying alloy parse failure.
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
}

/// Translate a `[[subscription]]` chain-log entry into an alloy `Filter`.
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
