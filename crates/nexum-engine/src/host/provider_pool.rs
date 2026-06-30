//! `nexum:host/chain` backend.
//!
//! Per-chain alloy provider, opened from the engine config at boot.
//! `request` is a raw JSON-RPC dispatch: the host hands `(method,
//! params)` straight to alloy's transport and returns the result body
//! verbatim. No method allowlist, no re-encoding of params - the
//! contract is "give us a JSON-RPC pair, we'll return what the node
//! returns".
//!
//! Transports:
//! - `ws://` / `wss://`  - `WsConnect`; required for `eth_subscribe`.
//! - `http://` / `https://` - alloy's HTTP transport; request/response only.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use alloy_provider::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Header, Log};
use futures::stream::Stream;
use futures::stream::StreamExt as _;
use serde_json::value::RawValue;
use thiserror::Error;
use tracing::info;

use crate::engine_config::EngineConfig;

/// Pool of alloy providers keyed by chain id.
#[derive(Debug, Clone)]
pub struct ProviderPool {
    providers: Arc<BTreeMap<u64, DynProvider>>,
}

impl ProviderPool {
    /// Open one provider per chain in `cfg.chains`. WebSocket URLs
    /// engage alloy's pubsub transport; HTTP URLs use the HTTP
    /// transport. Connection failures propagate to the caller; the
    /// engine treats them as fatal at boot.
    pub async fn from_config(cfg: &EngineConfig) -> Result<Self, ProviderError> {
        let mut providers: BTreeMap<u64, DynProvider> = BTreeMap::new();
        for (chain_id, chain_cfg) in &cfg.chains {
            let url = chain_cfg.rpc_url.as_str();
            info!(chain_id, url, "opening chain RPC provider");
            let provider = if url.starts_with("ws://") || url.starts_with("wss://") {
                ProviderBuilder::new()
                    .connect_ws(WsConnect::new(url))
                    .await
                    .map_err(|e| ProviderError::Connect {
                        chain_id: *chain_id,
                        detail: e.to_string(),
                    })?
                    .erased()
            } else {
                let parsed: url::Url =
                    url.parse()
                        .map_err(|e: url::ParseError| ProviderError::Connect {
                            chain_id: *chain_id,
                            detail: e.to_string(),
                        })?;
                ProviderBuilder::new().connect_http(parsed).erased()
            };
            providers.insert(*chain_id, provider);
        }
        Ok(Self {
            providers: Arc::new(providers),
        })
    }

    /// Empty pool - used by tests. Every `request` call returns
    /// `UnknownChain`.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            providers: Arc::new(BTreeMap::new()),
        }
    }

    /// Open a new-blocks (`eth_subscribe newHeads`) stream on
    /// `chain_id`. Requires a WS / IPC transport at construction
    /// time; HTTP-only providers surface `UnknownChain` here.
    pub async fn subscribe_blocks(&self, chain_id: u64) -> Result<BlockStream, ProviderError> {
        let provider = self
            .providers
            .get(&chain_id)
            .ok_or(ProviderError::UnknownChain(chain_id))?;
        let sub = provider
            .subscribe_blocks()
            .await
            .map_err(|e| ProviderError::Rpc {
                method: "eth_subscribe(newHeads)".into(),
                detail: e.to_string(),
            })?;
        let stream = sub.into_stream().map(Ok::<_, ProviderError>);
        Ok(Box::pin(stream))
    }

    /// Open an `eth_subscribe(logs, filter)` stream on `chain_id`.
    pub async fn subscribe_logs(
        &self,
        chain_id: u64,
        filter: Filter,
    ) -> Result<LogStream, ProviderError> {
        let provider = self
            .providers
            .get(&chain_id)
            .ok_or(ProviderError::UnknownChain(chain_id))?;
        let sub = provider
            .subscribe_logs(&filter)
            .await
            .map_err(|e| ProviderError::Rpc {
                method: "eth_subscribe(logs)".into(),
                detail: e.to_string(),
            })?;
        let stream = sub.into_stream().map(Ok::<_, ProviderError>);
        Ok(Box::pin(stream))
    }

    /// Raw JSON-RPC dispatch. `params_json` must be the JSON encoding
    /// of the params array (e.g. `"[\"0x...\",\"latest\"]"`), as
    /// produced by the SDK's `chain::request` glue.
    pub async fn request(
        &self,
        chain_id: u64,
        method: String,
        params_json: String,
    ) -> Result<String, ProviderError> {
        let provider = self
            .providers
            .get(&chain_id)
            .ok_or(ProviderError::UnknownChain(chain_id))?;
        // Pass the params through as a raw JSON value so alloy does
        // not re-encode them on the way to the node.
        let params: Box<RawValue> =
            RawValue::from_string(params_json).map_err(|e| ProviderError::InvalidParams {
                method: method.clone(),
                detail: e.to_string(),
            })?;
        // `raw_request` consumes the method name; clone once for the
        // error branch so the success path moves the original string
        // straight into alloy without an extra allocation.
        let method_for_err = method.clone();
        let result: Box<RawValue> =
            provider
                .raw_request(method.into(), params)
                .await
                .map_err(|e| ProviderError::Rpc {
                    method: method_for_err,
                    detail: e.to_string(),
                })?;
        Ok(result.get().to_owned())
    }
}

/// Boxed stream of `newHeads`-style block headers.
pub type BlockStream = Pin<Box<dyn Stream<Item = Result<Header, ProviderError>> + Send>>;
/// Boxed stream of `logs`-filtered log events.
pub type LogStream = Pin<Box<dyn Stream<Item = Result<Log, ProviderError>> + Send>>;

/// Errors surfaced by [`ProviderPool`].
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Chain id absent from the engine config.
    #[error("unknown chain {0} (no engine.toml entry)")]
    UnknownChain(u64),
    /// Could not open the underlying transport.
    #[error("connect chain {chain_id}: {detail}")]
    Connect {
        /// Chain id we failed to dial.
        chain_id: u64,
        /// Transport-side error string.
        detail: String,
    },
    /// The guest-supplied JSON params did not parse.
    #[error("invalid params JSON for `{method}`: {detail}")]
    InvalidParams {
        /// RPC method name.
        method: String,
        /// JSON-parser detail.
        detail: String,
    },
    /// The node returned an error for the dispatched call.
    #[error("rpc `{method}` failed: {detail}")]
    Rpc {
        /// RPC method name.
        method: String,
        /// Transport-side error string.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_pool_rejects_lookups() {
        let pool = ProviderPool::empty();
        let err = pool
            .request(1, "eth_blockNumber".into(), "[]".into())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::UnknownChain(1)));
    }

    #[tokio::test]
    async fn empty_pool_rejects_block_subscribe() {
        let pool = ProviderPool::empty();
        // Can't use .unwrap_err() because BlockStream doesn't impl Debug.
        assert!(matches!(
            pool.subscribe_blocks(1).await,
            Err(ProviderError::UnknownChain(1))
        ));
    }

    #[tokio::test]
    async fn empty_pool_rejects_log_subscribe() {
        let pool = ProviderPool::empty();
        let filter = alloy_rpc_types_eth::Filter::new();
        assert!(matches!(
            pool.subscribe_logs(1, filter).await,
            Err(ProviderError::UnknownChain(1))
        ));
    }

    #[tokio::test]
    async fn invalid_params_json_is_rejected_before_network() {
        // RawValue::from_string rejects non-JSON; verify the parse layer
        // we rely on before forwarding to alloy.
        let bad = "not json at all {{{";
        let result = RawValue::from_string(bad.to_owned());
        assert!(result.is_err(), "invalid JSON should fail RawValue parse");
    }
}
