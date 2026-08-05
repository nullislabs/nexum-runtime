//! In-process mock RPC transports behind the real [`ProviderPool`]:
//! [`MockRpc`] replays a FIFO response script and records every request;
//! [`FakeNode`] routes requests by method over settable head/block/log state.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_chains::Chain;
use alloy_json_rpc::{RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest};
use alloy_primitives::{B256, U64, U256};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rpc_client::ClientBuilder;
use alloy_rpc_types_eth::{Block, BlockNumberOrTag, Filter, FilterBlockOption, Header, Log};
use alloy_transport::mock::{Asserter, MockResponse, MockTransport};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use serde_json::value::RawValue;

use crate::host::component::ChainMethod;
use crate::host::provider_pool::ProviderPool;

/// One dispatched JSON-RPC request, captured in call order.
#[derive(Debug, Clone)]
pub struct CapturedRpc {
    /// RPC method name.
    pub method: String,
    /// Decoded params array; `Null` when the request carried none.
    pub params: serde_json::Value,
}

type Sink = Arc<Mutex<Vec<CapturedRpc>>>;

fn record(sink: &Sink, req: &SerializedRequest) {
    let params = req
        .params()
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or(serde_json::Value::Null);
    sink.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(CapturedRpc {
            method: req.method().to_owned(),
            params,
        });
}

fn record_packet(sink: &Sink, packet: &RequestPacket) {
    match packet {
        RequestPacket::Single(req) => record(sink, req),
        RequestPacket::Batch(reqs) => {
            for req in reqs {
                record(sink, req);
            }
        }
    }
}

struct CaptureLayer(Sink);

impl<S> tower::Layer<S> for CaptureLayer {
    type Service = CaptureService<S>;

    fn layer(&self, inner: S) -> CaptureService<S> {
        CaptureService {
            inner,
            sink: self.0.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct CaptureService<S> {
    inner: S,
    sink: Sink,
}

impl<S> tower::Service<RequestPacket> for CaptureService<S>
where
    S: tower::Service<RequestPacket, Response = ResponsePacket, Error = TransportError>,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        record_packet(&self.sink, &req);
        self.inner.call(req)
    }
}

/// FIFO-scripted transport; responses replay in push order regardless of
/// method.
#[derive(Clone, Default)]
pub struct MockRpc {
    asserter: Asserter,
    captured: Sink,
}

impl MockRpc {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider over the scripted transport, request capture included.
    pub fn provider(&self) -> DynProvider {
        let client = ClientBuilder::default()
            .layer(CaptureLayer(self.captured.clone()))
            .transport(MockTransport::new(self.asserter.clone()), true);
        ProviderBuilder::new().connect_client(client).erased()
    }

    /// Append a response script atomically.
    pub fn push_script(&self, items: impl IntoIterator<Item = MockResponse>) {
        self.asserter.write_q().extend(items);
    }

    /// Every request dispatched so far, in call order.
    pub fn captured(&self) -> Vec<CapturedRpc> {
        self.captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Responses still queued; `0` marks a phase boundary.
    pub fn pending(&self) -> usize {
        self.asserter.read_q().len()
    }

    /// `fromBlock` of every captured ranged `eth_getLogs`, in call order.
    pub fn log_range_froms(&self) -> Vec<u64> {
        self.captured()
            .iter()
            .filter(|req| req.method == "eth_getLogs")
            .filter_map(|req| {
                let from = req.params.get(0)?.get("fromBlock")?.as_str()?;
                u64::from_str_radix(from.trim_start_matches("0x"), 16).ok()
            })
            .collect()
    }
}

/// A successful response carrying `value` as its JSON result.
pub fn rpc_ok<T: serde::Serialize>(value: &T) -> MockResponse {
    let body = serde_json::to_string(value).expect("mock response serializes");
    MockResponse::Success(RawValue::from_string(body).expect("serialized JSON is a raw value"))
}

/// An `eth_blockNumber`-shaped head response.
pub fn rpc_head(number: u64) -> MockResponse {
    rpc_ok(&U64::from(number))
}

/// A JSON-RPC error response; terminal for alloy's canonical log stream.
pub fn rpc_err(msg: &str) -> MockResponse {
    MockResponse::Failure(alloy_json_rpc::ErrorPayload::internal_error_message(
        msg.to_owned().into(),
    ))
}

/// Deterministic block hash for `number`.
pub fn test_hash(number: u64) -> B256 {
    B256::from(U256::from(number).wrapping_add(U256::from(1u64) << 128))
}

/// A hash-chained block at `number`, parent-linked via [`test_hash`].
pub fn linked_block(number: u64) -> Block {
    let mut block: Block = Block::default();
    block.header.inner.number = number;
    block.header.hash = test_hash(number);
    block.header.inner.parent_hash = number.checked_sub(1).map(test_hash).unwrap_or_default();
    block
}

/// A pool of [`MockRpc`]-backed chains polling at `poll_interval`.
pub fn mocked_pool<'a>(
    chains: impl IntoIterator<Item = (Chain, &'a MockRpc)>,
    poll_interval: Duration,
) -> ProviderPool {
    ProviderPool::for_tests(
        chains
            .into_iter()
            .map(|(chain, rpc)| (chain, rpc.provider())),
        poll_interval,
    )
}

/// Method-routed fake RPC node serving pushed blocks and logs;
/// `eth_blockNumber` parks until a head exists.
#[derive(Clone, Default)]
pub struct FakeNode(Arc<FakeNodeInner>);

#[derive(Default)]
struct FakeNodeInner {
    state: Mutex<FakeNodeState>,
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct FakeNodeState {
    head: Option<u64>,
    blocks: BTreeMap<u64, Block>,
    by_hash: HashMap<B256, u64>,
    logs: HashMap<u64, Vec<Log>>,
    canned: HashMap<&'static str, String>,
    captured: Vec<CapturedRpc>,
    delay: Option<Duration>,
    fail_head_fetches: u32,
}

impl FakeNode {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, FakeNodeState> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A pool serving every chain in `chains` from this node.
    pub fn pool(&self, chains: &[Chain], poll_interval: Duration) -> ProviderPool {
        ProviderPool::for_tests(
            chains.iter().map(|&chain| (chain, self.provider())),
            poll_interval,
        )
    }

    /// A provider routed to this node.
    pub fn provider(&self) -> DynProvider {
        let client = ClientBuilder::default().transport(self.clone(), true);
        ProviderBuilder::new().connect_client(client).erased()
    }

    /// Serve `header`'s block and advance the head to it; a zero hash gets
    /// the deterministic [`test_hash`] chain.
    pub fn push_block(&self, header: Header) {
        let number = header.inner.number;
        let block = Block {
            header,
            ..Block::default()
        };
        self.insert_block(number, block);
    }

    /// Serve `log` and advance the head to its block; a log without a block
    /// number lands one past the current head.
    pub fn push_chain_log(&self, mut log: Log) {
        let number = log
            .block_number
            .unwrap_or_else(|| self.state().head.map_or(1, |h| h.saturating_add(1)));
        let hash = {
            let state = self.state();
            match state.blocks.get(&number) {
                Some(block) => block.header.hash,
                None => test_hash(number),
            }
        };
        log.block_number = Some(number);
        log.block_hash = Some(hash);
        {
            let mut state = self.state();
            state.logs.entry(number).or_default().push(log);
        }
        self.insert_block(number, linked_block(number));
    }

    fn insert_block(&self, number: u64, mut block: Block) {
        {
            let mut state = self.state();
            if state.blocks.contains_key(&number) {
                state.head = Some(state.head.map_or(number, |h| h.max(number)));
            } else {
                if block.header.hash == B256::ZERO {
                    block.header.hash = test_hash(number);
                }
                if block.header.inner.parent_hash == B256::ZERO {
                    block.header.inner.parent_hash =
                        number.checked_sub(1).map(test_hash).unwrap_or_default();
                }
                let hash = block.header.hash;
                state.by_hash.insert(hash, number);
                state.blocks.insert(number, block);
                state.head = Some(state.head.map_or(number, |h| h.max(number)));
            }
        }
        self.0.wake.notify_waiters();
    }

    /// Canned raw JSON result for `method`, served ahead of the built-in
    /// routing.
    pub fn on_method(&self, method: ChainMethod, result: impl Into<String>) -> &Self {
        self.state().canned.insert(method.as_str(), result.into());
        self
    }

    /// Park the next request for `delay` before serving it. One-shot.
    pub fn delay_next_request(&self, delay: Duration) {
        self.state().delay = Some(delay);
    }

    /// Fail the next `n` head fetches with a transport error.
    pub fn fail_head_fetches(&self, n: u32) {
        self.state().fail_head_fetches = n;
    }

    /// Every request dispatched so far, in call order.
    pub fn recorded_requests(&self) -> Vec<CapturedRpc> {
        self.state().captured.clone()
    }

    async fn serve(&self, req: SerializedRequest) -> Result<Response, TransportError> {
        let delay = {
            let mut state = self.state();
            let params = req
                .params()
                .and_then(|raw| serde_json::from_str(raw.get()).ok())
                .unwrap_or(serde_json::Value::Null);
            state.captured.push(CapturedRpc {
                method: req.method().to_owned(),
                params,
            });
            state.delay.take()
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let canned = self.state().canned.get(req.method()).cloned();
        if let Some(body) = canned {
            let raw = RawValue::from_string(body)
                .map_err(|e| TransportErrorKind::custom_str(&e.to_string()))?;
            return Ok(Response {
                id: req.id().clone(),
                payload: ResponsePayload::Success(raw),
            });
        }
        match req.method() {
            "eth_blockNumber" => loop {
                let notified = self.0.wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut state = self.state();
                    if state.fail_head_fetches > 0 {
                        state.fail_head_fetches -= 1;
                        return Err(TransportErrorKind::custom_str(
                            "scripted head fetch failure",
                        ));
                    }
                    if let Some(head) = state.head {
                        drop(state);
                        return respond(&req, &U64::from(head));
                    }
                }
                notified.as_mut().await;
            },
            "eth_getBlockByNumber" => {
                let (tag, _full): (BlockNumberOrTag, bool) = parse_params(&req)?;
                let state = self.state();
                let number = match tag {
                    BlockNumberOrTag::Number(n) => Some(n),
                    BlockNumberOrTag::Latest => state.head,
                    _ => None,
                };
                let block = number.and_then(|n| state.blocks.get(&n).cloned());
                drop(state);
                respond(&req, &block)
            }
            "eth_getLogs" => {
                let (filter,): (Filter,) = parse_params(&req)?;
                let state = self.state();
                let logs: Vec<Log> = match filter.block_option {
                    FilterBlockOption::AtBlockHash(hash) => state
                        .by_hash
                        .get(&hash)
                        .and_then(|n| state.logs.get(n).cloned())
                        .unwrap_or_default(),
                    FilterBlockOption::Range {
                        from_block,
                        to_block,
                    } => {
                        let from = from_block.and_then(|b| b.as_number()).unwrap_or(0);
                        let to = to_block
                            .and_then(|b| b.as_number())
                            .or(state.head)
                            .unwrap_or(0);
                        (from..=to)
                            .filter_map(|n| state.logs.get(&n))
                            .flatten()
                            .cloned()
                            .collect()
                    }
                };
                drop(state);
                respond(&req, &logs)
            }
            other => Err(TransportErrorKind::custom_str(&format!(
                "fake node has no route or canned response for {other}"
            ))),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(
    req: &SerializedRequest,
) -> Result<T, TransportError> {
    let raw = req.params().map(|raw| raw.get()).unwrap_or("[]");
    serde_json::from_str(raw).map_err(|e| TransportErrorKind::custom_str(&e.to_string()))
}

fn respond<T: serde::Serialize>(
    req: &SerializedRequest,
    value: &T,
) -> Result<Response, TransportError> {
    let body =
        serde_json::to_string(value).map_err(|e| TransportErrorKind::custom_str(&e.to_string()))?;
    let raw =
        RawValue::from_string(body).map_err(|e| TransportErrorKind::custom_str(&e.to_string()))?;
    Ok(Response {
        id: req.id().clone(),
        payload: ResponsePayload::Success(raw),
    })
}

impl tower::Service<RequestPacket> for FakeNode {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let node = self.clone();
        Box::pin(async move {
            match req {
                RequestPacket::Single(req) => node.serve(req).await.map(ResponsePacket::Single),
                RequestPacket::Batch(reqs) => {
                    let mut out = Vec::with_capacity(reqs.len());
                    for req in reqs {
                        out.push(node.serve(req).await?);
                    }
                    Ok(ResponsePacket::Batch(out))
                }
            }
        })
    }
}
