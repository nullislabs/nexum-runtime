//! `nexum:host/chain` backend: per-chain provider opened from the engine
//! config at boot.
//!
//! `request` is a raw JSON-RPC dispatch over a typed [`ChainMethod`], so only
//! the permitted read surface reaches the transport; params pass through
//! unencoded and the result body returns verbatim. WS/WSS push `newHeads`;
//! HTTP polls `eth_getBlockByNumber`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use alloy_chains::Chain;
use alloy_primitives::B256;
use alloy_provider::{CanonicalEvent, DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy_rpc_client::ClientBuilder;
use alloy_rpc_types_eth::{Filter, Header, Log};
use alloy_transport::layers::RetryBackoffLayer;
use alloy_transport::{RpcError, TransportError};
use anyhow::Context as _;
use futures::stream::Stream;
use futures::stream::StreamExt as _;
use serde_json::value::RawValue;
use thiserror::Error;
use tracing::info;

use crate::engine_config::EngineConfig;
use crate::host::component::ChainMethod;

/// Head re-poll cadence for chains without a block-time hint; known chains
/// derive it from [`Chain::average_blocktime_hint`].
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Transport retry-layer parameters; heal transient RPC blips below the
/// poller so a node hiccup does not force a re-open.
const RPC_MAX_RETRIES: u32 = 10;
const RPC_RETRY_BACKOFF_MS: u64 = 300;
/// Compute-units-per-second budget for rate-limited nodes; generous, this
/// pool is read-only and low-QPS.
const RPC_RETRY_CUPS: u64 = 100;

/// Transport retry layer applied to every provider in the pool.
fn retry_layer() -> RetryBackoffLayer {
    RetryBackoffLayer::new(RPC_MAX_RETRIES, RPC_RETRY_BACKOFF_MS, RPC_RETRY_CUPS)
}

/// One chain's opened provider plus how to drive it.
#[derive(Debug, Clone)]
struct ChainEndpoint {
    provider: DynProvider,
    timeout: Duration,
    /// WS/IPC drives block following by pubsub; HTTP polls.
    supports_pubsub: bool,
}

/// Keyed by chain; a missing entry is [`PoolError::UnknownChain`].
#[derive(Debug, Clone)]
pub struct ProviderPool {
    providers: Arc<HashMap<Chain, ChainEndpoint>>,
    /// In-flight `eth_getLogs` groups during gap backfill; `0` clamps to `1`.
    log_backfill_concurrency: usize,
    /// Test-only poll cadence override; `None` derives from the chain hint.
    poll_interval_override: Option<Duration>,
}

impl ProviderPool {
    /// Open one provider per chain in `cfg.chains`; connection failures
    /// propagate and are fatal at boot.
    pub async fn from_config(cfg: &EngineConfig) -> anyhow::Result<Self> {
        let mut providers: HashMap<Chain, ChainEndpoint> = HashMap::new();
        // Sort by numeric id so the boot logs are deterministic
        // (`Chain` is not `Ord`).
        let mut entries: Vec<_> = cfg.chains.iter().collect();
        entries.sort_by_key(|(c, _)| c.id());
        for (chain, chain_cfg) in entries {
            let endpoint = &chain_cfg.rpc_url;
            info!(
                chain_id = chain.id(),
                url = %endpoint,
                "opening chain RPC provider",
            );
            let timeout = Duration::from_secs(chain_cfg.request_timeout_secs);
            let supports_pubsub = endpoint.supports_pubsub();
            let provider = if supports_pubsub {
                // WS has no client-level timeout; only `request` bounds its calls.
                let client = ClientBuilder::default()
                    .layer(retry_layer())
                    .ws(WsConnect::new(endpoint.unredacted_dial_url().as_str()))
                    .await
                    .with_context(|| format!("connect chain {chain}"))?;
                ProviderBuilder::new().connect_client(client).erased()
            } else {
                let http = reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .with_context(|| format!("connect chain {chain}"))?;
                let client = ClientBuilder::default()
                    .layer(retry_layer())
                    .http_with_client(http, endpoint.unredacted_dial_url().clone());
                ProviderBuilder::new().connect_client(client).erased()
            };
            providers.insert(
                *chain,
                ChainEndpoint {
                    provider,
                    timeout,
                    supports_pubsub,
                },
            );
        }
        Ok(Self {
            providers: Arc::new(providers),
            log_backfill_concurrency: cfg.engine.log_backfill_concurrency,
            poll_interval_override: None,
        })
    }

    /// Empty pool; every `request` returns `UnknownChain`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn empty() -> Self {
        Self {
            providers: Arc::new(HashMap::new()),
            log_backfill_concurrency: 16,
            poll_interval_override: None,
        }
    }

    /// Log fetches are serial for deterministic RPC order.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_tests(
        providers: impl IntoIterator<Item = (Chain, DynProvider)>,
        poll_interval: Duration,
    ) -> Self {
        let providers = providers
            .into_iter()
            .map(|(chain, provider)| {
                (
                    chain,
                    ChainEndpoint {
                        provider,
                        timeout: Duration::from_secs(30),
                        supports_pubsub: false,
                    },
                )
            })
            .collect();
        Self {
            providers: Arc::new(providers),
            log_backfill_concurrency: 1,
            poll_interval_override: Some(poll_interval),
        }
    }

    fn poll_interval(&self, chain: Chain) -> Duration {
        self.poll_interval_override.unwrap_or_else(|| {
            chain
                .average_blocktime_hint()
                .unwrap_or(DEFAULT_POLL_INTERVAL)
        })
    }

    /// Follow canonical block headers on `chain`: WS via
    /// `eth_subscribe(newHeads)`, HTTP by polling at the chain's block time.
    pub async fn subscribe_blocks(&self, chain: Chain) -> Result<BlockStream, PoolError> {
        let ep = self
            .providers
            .get(&chain)
            .ok_or(PoolError::UnknownChain(chain))?;
        if ep.supports_pubsub {
            let sub = ep.provider.subscribe_blocks().await?;
            let stream = sub.into_stream().map(Ok::<_, TransportError>);
            return Ok(Box::pin(stream));
        }
        // Same-height replacements are not re-emitted.
        let head = ep.provider.get_block_number().await?;
        let stream = ep
            .provider
            .watch_blocks_from(head)
            .poll_interval(self.poll_interval(chain))
            .into_stream()
            .buffered(self.log_backfill_concurrency.max(1))
            .map(|item| item.map(|block| block.header));
        Ok(Box::pin(stream))
    }

    /// The alloy provider for one chain. Unrelated to a nexum service:
    /// this is the RPC transport, not a component.
    pub fn provider(&self, chain: Chain) -> Result<&DynProvider, PoolError> {
        self.providers
            .get(&chain)
            .map(|ep| &ep.provider)
            .ok_or(PoolError::UnknownChain(chain))
    }

    /// Canonical (reorg-aware) log stream on `chain` from `start_block`. Each
    /// item is one block's batch (possibly with no logs); reorg rollbacks
    /// carry `removed == true`.
    pub fn watch_chain_logs(
        &self,
        chain: Chain,
        filter: Filter,
        start_block: u64,
    ) -> Result<CanonicalLogStream, PoolError> {
        let ep = self
            .providers
            .get(&chain)
            .ok_or(PoolError::UnknownChain(chain))?;
        let stream = ep
            .provider
            .watch_canonical_logs_from(start_block, &filter)
            .rpc_concurrency(self.log_backfill_concurrency)
            .poll_interval(self.poll_interval(chain))
            .into_stream()
            .map(|item| {
                item.map(|event| {
                    // The poller stamps `removed` on each log already.
                    let (removed, block_logs) = match event {
                        CanonicalEvent::Added(block_logs) => (false, block_logs),
                        CanonicalEvent::Removed(block_logs) => (true, block_logs),
                    };
                    CanonicalLogBatch {
                        number: block_logs.block.header.number,
                        hash: block_logs.block.header.hash,
                        removed,
                        logs: block_logs.logs,
                    }
                })
            });
        Ok(Box::pin(stream))
    }

    /// Raw JSON-RPC dispatch; `params_json` is the JSON-encoded params array.
    pub async fn request(
        &self,
        chain: Chain,
        method: ChainMethod,
        params_json: String,
    ) -> Result<String, PoolError> {
        let ep = self
            .providers
            .get(&chain)
            .ok_or(PoolError::UnknownChain(chain))?;
        let name = method.as_str();
        // Raw JSON passthrough so alloy does not re-encode; `SerError` is the
        // variant alloy's retry layer treats as terminal.
        let params: Box<RawValue> = RawValue::from_string(params_json)
            .map_err(|source| PoolError::Rpc(RpcError::SerError(source)))?;
        let result: Box<RawValue> = tokio::time::timeout(
            ep.timeout,
            ep.provider.raw_request(Cow::Borrowed(name), params),
        )
        .await
        .map_err(|_| PoolError::Timeout)??;
        // Unbox the raw result into the returned String without
        // copying the body; the WIT boundary copy is the only one left.
        Ok(String::from(Box::<str>::from(result)))
    }
}

/// Boxed stream of `newHeads`-style block headers.
pub type BlockStream = Pin<Box<dyn Stream<Item = Result<Header, TransportError>> + Send>>;
/// One block's filter-matching logs; a block with no matching logs still
/// yields a batch.
#[derive(Debug, Clone)]
pub struct CanonicalLogBatch {
    /// Block height the batch was fetched at.
    pub number: u64,
    /// Canonical block hash the batch was fetched against.
    pub hash: B256,
    /// Reorg rollback: the block left the canonical chain.
    pub removed: bool,
    /// Matching logs with `removed` stamped; empty for a non-matching block.
    pub logs: Vec<Log>,
}

/// Boxed canonical per-block log stream; reorg rollbacks carry
/// `removed == true`.
pub type CanonicalLogStream =
    Pin<Box<dyn Stream<Item = Result<CanonicalLogBatch, TransportError>> + Send>>;

/// RPC failures pass through alloy's typed error, classified at the WIT edge.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PoolError {
    /// Chain absent from the engine config.
    #[error("unknown chain {0} (no engine.toml entry)")]
    UnknownChain(Chain),
    /// The configured per-request timeout elapsed.
    #[error("rpc request timed out")]
    Timeout,
    /// Anything alloy's transport reports: connect, decode, or a node
    /// error response. Classified into a fault at the WIT edge.
    #[error(transparent)]
    Rpc(#[from] TransportError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_pool_rejects_lookups() {
        let pool = ProviderPool::empty();
        let err = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .unwrap_err();
        assert!(matches!(err, PoolError::UnknownChain(c) if c == Chain::from_id(1)));
    }

    #[tokio::test]
    async fn empty_pool_rejects_block_subscribe() {
        let pool = ProviderPool::empty();
        // Can't use .unwrap_err() because BlockStream doesn't impl Debug.
        assert!(matches!(
            pool.subscribe_blocks(Chain::from_id(1)).await,
            Err(PoolError::UnknownChain(c)) if c == Chain::from_id(1)
        ));
    }

    #[test]
    fn empty_pool_rejects_provider_lookup() {
        let pool = ProviderPool::empty();
        assert!(matches!(
            pool.provider(Chain::from_id(1)),
            Err(PoolError::UnknownChain(c)) if c == Chain::from_id(1)
        ));
    }

    #[test]
    fn empty_pool_rejects_watch_chain_logs() {
        let pool = ProviderPool::empty();
        let filter = alloy_rpc_types_eth::Filter::new();
        // Can't use .unwrap_err() because CanonicalLogStream doesn't impl Debug.
        assert!(matches!(
            pool.watch_chain_logs(Chain::from_id(1), filter, 0),
            Err(PoolError::UnknownChain(c)) if c == Chain::from_id(1)
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

    /// Helper: build an `EngineConfig` with a single HTTP chain entry.
    fn test_config(chain: Chain, rpc_url: &str) -> EngineConfig {
        test_config_with_timeout(chain, rpc_url, 30)
    }

    /// As [`test_config`], with an explicit per-request timeout.
    fn test_config_with_timeout(chain: Chain, rpc_url: &str, timeout_secs: u64) -> EngineConfig {
        use crate::engine_config::{ChainConfig, EngineConfig, RpcEndpoint};
        let mut chains = HashMap::new();
        chains.insert(
            chain,
            ChainConfig {
                rpc_url: RpcEndpoint::try_from(rpc_url).expect("test rpc url parses"),
                request_timeout_secs: timeout_secs,
            },
        );
        EngineConfig {
            chains,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn invalid_params_through_request_produces_error() {
        let cfg = test_config(Chain::from_id(1), "http://127.0.0.1:1");
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let err = pool
            .request(
                Chain::from_id(1),
                ChainMethod::EthBlockNumber,
                "not json {{{".into(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PoolError::Rpc(RpcError::SerError(_))),
            "expected SerError, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rpc_error_on_unreachable_node() {
        let cfg = test_config(Chain::from_id(1), "http://127.0.0.1:1");
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let err = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PoolError::Rpc(_)),
            "expected Rpc error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn request_returns_result_body_verbatim() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        // The raw `result` bytes must come back byte-identical: no
        // re-encoding, no DOM round trip, quotes preserved.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"jsonrpc":"2.0","id":0,"result":{"number":"0x10","extra":[1,2]}}"#,
            ))
            .mount(&server)
            .await;

        let cfg = test_config(Chain::from_id(1), &server.uri());
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let body = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .unwrap();
        assert_eq!(body, r#"{"number":"0x10","extra":[1,2]}"#);
    }

    #[tokio::test]
    async fn rpc_error_on_malformed_node_response() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let cfg = test_config(Chain::from_id(1), &server.uri());
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let err = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PoolError::Rpc(_)),
            "expected Rpc error from malformed response, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn request_times_out_when_node_hangs() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        let server = MockServer::start().await;
        // Respond after 60 s - the pool is configured with a 1 s timeout,
        // so `raw_request` is cancelled well before the body arrives. The
        // large gap keeps the test from flaking on slow CI runners.
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(60))
                    .set_body_string(r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#),
            )
            .mount(&server)
            .await;

        let cfg = test_config_with_timeout(Chain::from_id(1), &server.uri(), 1);
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let started = std::time::Instant::now();
        let err = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .unwrap_err();
        // The reqwest and tokio timeouts race; whichever wins, the guest
        // must see the dedicated timeout fault.
        let chain_err = crate::bindings::nexum::host::chain::ChainError::from(err);
        assert!(
            matches!(
                chain_err,
                crate::bindings::nexum::host::chain::ChainError::Fault(
                    crate::bindings::nexum::host::types::Fault::Timeout
                )
            ),
            "expected a timeout fault, got: {chain_err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "request must fail at the configured timeout, not the 60 s hang",
        );
    }

    #[tokio::test]
    async fn hung_transport_fails_native_probe_within_timeout() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        // No tokio timeout wraps the probe; the client-level timeout must bound it.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(60))
                    .set_body_string(r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#),
            )
            .mount(&server)
            .await;

        let cfg = test_config_with_timeout(Chain::from_id(1), &server.uri(), 1);
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        let started = std::time::Instant::now();
        let result = pool
            .provider(Chain::from_id(1))
            .unwrap()
            .get_block_number()
            .await;
        assert!(result.is_err(), "hung node must not yield a head");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "probe must fail within the configured timeout, not the 60 s hang",
        );
    }

    #[tokio::test]
    async fn http_config_block_subscribe_takes_poll_path() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        // An HTTP transport has no pubsub, so `subscribe_blocks` must fall
        // back to polling rather than erroring. The head fetch
        // (`eth_blockNumber`) is the only call made at setup - the block
        // poller stream is lazy - so one mocked response proves the poll
        // path opens cleanly.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"jsonrpc":"2.0","id":0,"result":"0x10"}"#),
            )
            .mount(&server)
            .await;

        let cfg = test_config(Chain::from_id(1), &server.uri());
        let pool = ProviderPool::from_config(&cfg).await.unwrap();
        // BlockStream doesn't impl Debug, so assert on `is_ok` rather than
        // unwrapping.
        assert!(
            pool.subscribe_blocks(Chain::from_id(1)).await.is_ok(),
            "http config should open the block poll path without erroring",
        );
    }
}
