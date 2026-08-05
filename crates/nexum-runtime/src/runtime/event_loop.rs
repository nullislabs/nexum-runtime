//! Open live chain event sources and dispatch their events to the supervisor
//! until shutdown. Blocks come from `eth_subscribe(newHeads)` (WS); chain-logs
//! from an `eth_getLogs` block-range poller that re-queries the reconnect gap
//! and retracts a reorged delivered tail.
//!
//! `open_block_streams` and `open_chain_log_streams` each spawn one
//! reconnect-aware task per subscription: it opens the stream, pumps items to
//! an mpsc channel, and on drop waits `restart_policy::backoff_for` before
//! reopening, resetting the backoff once the stream has been healthy for
//! `HEALTHY_WINDOW`. The tasks exit with [`TaskExit::ReceiverGone`] when `run`
//! drops the receivers; their handles collect into a [`TaskSet`] the loop
//! drains on shutdown.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_chains::Chain;
use alloy_primitives::B256;
use alloy_provider::Provider as _;
use alloy_transport::TransportError;
use futures::StreamExt;
use futures::stream::{BoxStream, select_all};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::bindings::nexum;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{ExtensionEvent, ExtensionEventStream};
use crate::host::provider_pool::ProviderPool;
use crate::module_id::ModuleId;
use crate::runtime::restart_policy::backoff_for;
use crate::supervisor::{ChainLogSub, Supervisor};
use nexum_tasks::{TaskExecutor, TaskExit, TaskSet};

/// Uninterrupted-event duration before the backoff counter resets to 0.
const HEALTHY_WINDOW: Duration = Duration::from_secs(60);

/// Silence between block events beyond which the next event logs a gap-closed
/// line, surfacing an alloy-internal transport reconnect that produced no
/// `stream ended` event.
const BLOCK_GAP_LOG_THRESHOLD: Duration = Duration::from_secs(60);

/// Channel buffer for each reconnect task.
const RECONNECT_CHANNEL_BUF: usize = 64;

/// Block-gap size at or above which a re-open logs a large-backfill notice.
const LARGE_GAP_LOG_THRESHOLD: u64 = 1_000;

/// Open one reconnect-aware block-subscription task per chain, spawned via
/// `executor` with handles pushed into `tasks` for graceful shutdown.
pub fn open_block_streams(
    pool: &ProviderPool,
    chains: &[Chain],
    executor: &TaskExecutor,
    tasks: &mut TaskSet,
) -> Vec<TaggedBlockStream> {
    let mut streams = Vec::new();
    for &chain in chains {
        let (tx, rx) = mpsc::channel::<
            Result<(Chain, alloy_rpc_types_eth::Header), (Chain, TransportError)>,
        >(RECONNECT_CHANNEL_BUF);
        let pool = pool.clone();
        tasks.push(executor.spawn(reconnecting_block_task(pool, chain, tx)));
        let tagged: TaggedBlockStream = Box::pin(receiver_stream(rx));
        streams.push(tagged);
    }
    streams
}

/// Open one reconnect-aware chain-log task per subscription; see
/// [`open_block_streams`].
pub fn open_chain_log_streams(
    pool: &ProviderPool,
    subs: Vec<ChainLogSub>,
    executor: &TaskExecutor,
    tasks: &mut TaskSet,
) -> Vec<TaggedChainLogStream> {
    let mut streams = Vec::new();
    for sub in subs {
        let (tx, rx) = mpsc::channel::<TaggedChainLog>(RECONNECT_CHANNEL_BUF);
        let pool = pool.clone();
        let resume = ChainLogResume {
            // The cursor key is constant per subscription and cloned onto every
            // log; `Arc` keeps that clone cheap.
            cursor_key: sub.cursor_key.map(Arc::from),
            initial_cursor: sub.initial_cursor,
            max_lookback: sub.max_lookback,
        };
        tasks.push(executor.spawn(reconnecting_chain_log_task(
            pool, sub.module, sub.chain, sub.filter, resume, tx,
        )));
        let tagged: TaggedChainLogStream = Box::pin(receiver_stream(rx));
        streams.push(tagged);
    }
    streams
}

/// Wrap an `mpsc::Receiver<T>` as a `Stream<Item = T>`.
fn receiver_stream<T: Send + 'static>(
    rx: mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> + Send {
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

/// Reconnect-aware loop for one chain's block subscription: re-opens the
/// stream with exponential backoff after every drop or error.
async fn reconnecting_block_task(
    pool: ProviderPool,
    chain: Chain,
    tx: mpsc::Sender<Result<(Chain, alloy_rpc_types_eth::Header), (Chain, TransportError)>>,
) -> TaskExit {
    let chain_id = chain.id();
    let mut attempt: u32 = 0;
    let mut last_event: Option<Instant> = None;
    loop {
        match pool.subscribe_blocks(chain).await {
            Ok(mut inner) => {
                if attempt == 0 {
                    info!(chain_id, "block subscription open");
                } else {
                    info!(chain_id, attempt, "block subscription reopened");
                    metrics::counter!(
                        "nexum_runtime_stream_reconnects_total",
                        "kind" => "block",
                        "chain_id" => chain_id.to_string(),
                    )
                    .increment(1);
                }
                while let Some(item) = inner.next().await {
                    let now = Instant::now();
                    if attempt > 0
                        && last_event.is_some_and(|t| now.duration_since(t) >= HEALTHY_WINDOW)
                    {
                        info!(chain_id, "block stream healthy - resetting backoff");
                        attempt = 0;
                    }
                    // Detect transport-layer reconnects that
                    // alloy handled internally - `inner.next().await`
                    // keeps yielding events but with a long gap. The
                    // engine's reconnect path (`stream ended` -> wait
                    // backoff -> `subscription reopened`) does not fire
                    // for these, so without this log a soak operator
                    // sees an `alloy_transport_ws::native` ERROR
                    // followed by silence indistinguishable from a
                    // hung engine.
                    if let Some(gap) =
                        block_stream_gap_to_log(now, last_event, BLOCK_GAP_LOG_THRESHOLD)
                    {
                        let gap_s = gap.as_secs();
                        info!(
                            chain_id,
                            gap_s,
                            kind = "block",
                            "stream gap closed - first event after silence \
                             (likely an alloy-internal transport reconnect)"
                        );
                    }
                    last_event = Some(now);
                    let tagged = item
                        .map(|header| (chain, header))
                        .map_err(|err| (chain, err));
                    if tx.send(tagged).await.is_err() {
                        // Receiver dropped -> engine shutting down.
                        return TaskExit::ReceiverGone;
                    }
                }
                warn!(chain_id, "block stream ended (WebSocket dropped?)");
                attempt = attempt.saturating_add(1);
            }
            Err(err) => {
                warn!(chain_id, error = %err, "block subscription failed");
                attempt = attempt.saturating_add(1);
            }
        }
        let backoff = backoff_for(attempt);
        warn!(
            chain_id,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "reconnecting block subscription after backoff",
        );
        tokio::time::sleep(backoff).await;
    }
}

/// Per-subscription resume and backfill knobs for a chain-log task.
struct ChainLogResume {
    /// Durable cursor key; `Some` for a `resume` subscription.
    cursor_key: Option<Arc<str>>,
    /// Persisted resume block read at boot; the first successful open starts here.
    initial_cursor: Option<u64>,
    /// Opt-in cap in blocks on backfill depth; `None` backfills the whole gap.
    max_lookback: Option<u64>,
}

/// Last delivered log-bearing height, kept so a re-open can retract it if it
/// left the canonical chain.
struct DeliveredTail {
    number: u64,
    hash: B256,
    logs: Vec<alloy_rpc_types_eth::Log>,
}

/// Poller-backed loop for one (module, chain) chain-log subscription; a
/// re-open resumes past the scanned range and retracts a reorged tail.
async fn reconnecting_chain_log_task(
    pool: ProviderPool,
    module: ModuleId,
    chain: Chain,
    filter: alloy_rpc_types_eth::Filter,
    resume: ChainLogResume,
    tx: mpsc::Sender<TaggedChainLog>,
) -> TaskExit {
    let ChainLogResume {
        cursor_key,
        initial_cursor,
        max_lookback,
    } = resume;
    let chain_id = chain.id();
    let mut attempt: u32 = 0;
    let mut last_event: Option<Instant> = None;
    // One past the highest scanned height; rolled back on a removed batch.
    let mut resume_from: Option<u64> = None;
    let mut tail: Option<DeliveredTail> = None;
    // Cleared only once an open succeeds.
    let mut boot_resume: Option<u64> = initial_cursor;
    loop {
        let provider = match pool.provider(chain) {
            Ok(provider) => provider,
            Err(err) => {
                attempt = attempt.saturating_add(1);
                let backoff = backoff_for(attempt);
                warn!(
                    module = %module,
                    chain_id,
                    error = %err,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "chain-log provider lookup failed - retrying after backoff",
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
        };
        let head = match provider.get_block_number().await {
            Ok(head) => head,
            Err(err) => {
                attempt = attempt.saturating_add(1);
                let backoff = backoff_for(attempt);
                warn!(
                    module = %module,
                    chain_id,
                    error = %err,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "chain-log head fetch failed - retrying after backoff",
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
        };
        // An unconfirmed tail hash is a failed open, never a retraction.
        let mut invalidated_tail: Option<u64> = None;
        if let Some(t) = &tail {
            match provider.get_block_by_number(t.number.into()).await {
                Ok(Some(block)) if block.header.hash == t.hash => {}
                Ok(Some(_)) => invalidated_tail = Some(t.number),
                Ok(None) | Err(_) => {
                    attempt = attempt.saturating_add(1);
                    let backoff = backoff_for(attempt);
                    warn!(
                        module = %module,
                        chain_id,
                        tail_block = t.number,
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        "chain-log tail hash unconfirmed - retrying after backoff",
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }
        }
        let mut start_block = poller_start_block(boot_resume, resume_from, invalidated_tail, head);
        // Opt-in bound: `max_lookback` caps how far back a resume
        // subscription backfills. The default (`None`) backfills fully; a
        // set cap clamps the start up to `head - cap` and surfaces the
        // dropped oldest blocks.
        if let Some(cap) = max_lookback {
            let floor = head.saturating_sub(cap);
            if start_block < floor {
                warn!(
                    module = %module,
                    chain_id,
                    skipped_from = start_block,
                    skipped_to = floor,
                    "chain-log gap exceeds max_lookback - skipping the oldest missed blocks",
                );
                start_block = floor;
            }
        }
        // A large gap is backfilled in full (never skipped); surface it so a long
        // catch-up is visible rather than looking like a stall.
        if head.saturating_sub(start_block) >= LARGE_GAP_LOG_THRESHOLD {
            info!(
                module = %module,
                chain_id,
                from = start_block,
                to = head,
                blocks = head.saturating_sub(start_block),
                "chain-log poller backfilling a large gap"
            );
        }
        match pool.watch_chain_logs(chain, filter.clone(), start_block) {
            Ok(mut inner) => {
                if attempt == 0 {
                    info!(module = %module, chain_id, start_block, "chain-log poller open");
                } else {
                    info!(
                        module = %module,
                        chain_id,
                        attempt,
                        start_block,
                        "chain-log poller reopened"
                    );
                    metrics::counter!(
                        "nexum_runtime_stream_reconnects_total",
                        "kind" => "chain-log",
                        "chain_id" => chain_id.to_string(),
                        "module" => module.to_string(),
                    )
                    .increment(1);
                }
                // An itemless open re-opens at the same block.
                boot_resume = None;
                resume_from = Some(start_block);
                // Retract before pumping; the stream restates the height.
                if invalidated_tail.is_some()
                    && let Some(t) = tail.take()
                {
                    warn!(
                        module = %module,
                        chain_id,
                        tail_block = t.number,
                        "chain-log tail reorged while disconnected - retracting its logs",
                    );
                    for mut log in t.logs {
                        log.removed = true;
                        let tagged = (module.clone(), chain, log, cursor_key.clone());
                        if tx.send(tagged).await.is_err() {
                            return TaskExit::ReceiverGone;
                        }
                    }
                }
                while let Some(item) = inner.next().await {
                    let now = Instant::now();
                    if attempt > 0
                        && last_event.is_some_and(|t| now.duration_since(t) >= HEALTHY_WINDOW)
                    {
                        info!(
                            module = %module,
                            chain_id,
                            "chain-log stream healthy - resetting backoff"
                        );
                        attempt = 0;
                    }
                    last_event = Some(now);
                    match item {
                        // Each log arrives with `removed` already stamped.
                        Ok(batch) => {
                            for log in &batch.logs {
                                let tagged =
                                    (module.clone(), chain, log.clone(), cursor_key.clone());
                                if tx.send(tagged).await.is_err() {
                                    return TaskExit::ReceiverGone;
                                }
                            }
                            if batch.removed {
                                // A rollback un-scans the height and drops a
                                // tail at or above it.
                                resume_from =
                                    Some(resume_from.map_or(batch.number, |r| r.min(batch.number)));
                                if tail.as_ref().is_some_and(|t| t.number >= batch.number) {
                                    tail = None;
                                }
                            } else {
                                // Empty batches advance the basis too.
                                let next = batch.number.saturating_add(1);
                                resume_from = Some(resume_from.map_or(next, |r| r.max(next)));
                                if !batch.logs.is_empty() {
                                    tail = Some(DeliveredTail {
                                        number: batch.number,
                                        hash: batch.hash,
                                        logs: batch.logs,
                                    });
                                }
                            }
                        }
                        // A poller error is terminal for the alloy stream;
                        // break to re-open from a fresh head rather than
                        // pumping a dead stream.
                        Err(err) => {
                            warn!(
                                module = %module,
                                chain_id,
                                error = %err,
                                "chain-log poller error - reopening"
                            );
                            break;
                        }
                    }
                }
                warn!(module = %module, chain_id, "chain-log poller stream ended - reopening");
                attempt = attempt.saturating_add(1);
            }
            Err(err) => {
                warn!(
                    module = %module,
                    chain_id,
                    error = %err,
                    "chain-log poller open failed"
                );
                attempt = attempt.saturating_add(1);
            }
        }
        let backoff = backoff_for(attempt);
        warn!(
            module = %module,
            chain_id,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "reconnecting chain-log poller after backoff",
        );
        tokio::time::sleep(backoff).await;
    }
}

pub type TaggedBlockStream = std::pin::Pin<
    Box<
        dyn futures::Stream<
                Item = Result<(Chain, alloy_rpc_types_eth::Header), (Chain, TransportError)>,
            > + Send,
    >,
>;
/// One tagged chain-log item: `(module, chain, log, cursor_key)`;
/// `cursor_key` is `Some` for a `resume` subscription.
pub type TaggedChainLog = (ModuleId, Chain, alloy_rpc_types_eth::Log, Option<Arc<str>>);
pub type TaggedChainLogStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = TaggedChainLog> + Send>>;
/// Drive the supervisor with events until `shutdown` resolves.
///
/// `shutdown` is observed only between dispatches, never mid-`call_on_event`,
/// so an in-flight wasmtime call finishes before the loop exits; the guard it
/// yields is held until return, so the drain covers the final dispatch and
/// cursor commit. Returns the `(blocks, chain_logs)` dispatch tally.
pub async fn run<T: RuntimeTypes, G>(
    supervisor: &mut Supervisor<T>,
    block_streams: Vec<TaggedBlockStream>,
    chain_log_streams: Vec<TaggedChainLogStream>,
    extension_streams: Vec<ExtensionEventStream>,
    tasks: TaskSet,
    shutdown: impl std::future::Future<Output = G> + Send,
) -> (u64, u64) {
    // `select_all` over an empty Vec yields `None` immediately, which
    // would trip the "stream ended -> shut down" arm below before the
    // first block / chain-log ever flows. Engine configs that subscribe to
    // only one event kind (e.g. all modules use `[[subscription]] kind
    // = "block"`) are valid and must not be punished. Replace each
    // empty side with `stream::pending()` so the corresponding select
    // arm is never selected; the bail-on-None semantic still fires
    // when a *non-empty* stream actually closes.
    let mut blocks: BoxStream<'_, _> = if block_streams.is_empty() {
        futures::stream::pending().boxed()
    } else {
        select_all(block_streams).boxed()
    };
    let mut chain_logs: BoxStream<'_, _> = if chain_log_streams.is_empty() {
        futures::stream::pending().boxed()
    } else {
        select_all(chain_log_streams).boxed()
    };
    let mut extension_events: BoxStream<'_, _> = if extension_streams.is_empty() {
        futures::stream::pending().boxed()
    } else {
        select_all(extension_streams).boxed()
    };
    let mut shutdown = Box::pin(shutdown);
    let mut dispatched_blocks: u64 = 0;
    let mut dispatched_chain_logs: u64 = 0;
    let mut dispatched_extension_events: u64 = 0;
    let started = Instant::now();
    loop {
        // Phase 1: pick the next event OR observe shutdown. The
        // dispatch itself happens in phase 2 (outside the select)
        // so an in-flight wasmtime call never gets cancelled by a
        // shutdown signal arriving mid-dispatch.
        enum NextEvent<G> {
            Block(nexum::host::types::Block),
            // The alloy `Log` is boxed so the `Chain` tag does not push
            // the enum past the large-variant lint threshold.
            ChainLog(
                ModuleId,
                Chain,
                Box<alloy_rpc_types_eth::Log>,
                Option<Arc<str>>,
            ),
            Extension(ExtensionEvent),
            // Carries the drain guard `shutdown` yielded.
            Shutdown(G),
            StreamPanic(&'static str),
        }
        let next = tokio::select! {
            biased;
            guard = &mut shutdown => NextEvent::Shutdown(guard),
            next = blocks.next() => match next {
                Some(Ok((chain, header))) => NextEvent::Block(nexum::host::types::Block {
                    chain_id: chain.id(),
                    number: header.number,
                    hash: header.hash.as_slice().to_vec(),
                    timestamp: header.timestamp.saturating_mul(1000),
                }),
                Some(Err((chain, err))) => {
                    warn!(chain_id = chain.id(), error = %err, "block stream error - continuing");
                    continue;
                }
                None => NextEvent::StreamPanic("block"),
            },
            next = chain_logs.next() => match next {
                Some((module, chain, log, cursor_key)) => {
                    NextEvent::ChainLog(module, chain, Box::new(log), cursor_key)
                }
                None => NextEvent::StreamPanic("chain-log"),
            },
            next = extension_events.next() => match next {
                Some(event) => NextEvent::Extension(event),
                // Extension source tasks loop forever; `None` means one exited.
                None => NextEvent::StreamPanic("extension-event"),
            },
        };

        match next {
            NextEvent::Block(block) => {
                supervisor.dispatch_block(block).await;
                dispatched_blocks += 1;
            }
            NextEvent::ChainLog(module, chain, log, cursor_key) => {
                supervisor
                    .dispatch_chain_log(&module, chain, *log, cursor_key.as_deref())
                    .await;
                dispatched_chain_logs += 1;
            }
            NextEvent::Extension(event) => {
                supervisor.dispatch_extension_event(event).await;
                dispatched_extension_events += 1;
            }
            NextEvent::Shutdown(guard) => {
                // Drop the stream-end receivers so the reconnect
                // tasks observe a closed channel and exit. Then drain
                // the task set so the engine genuinely sees the tasks
                // finish before returning.
                drop(blocks);
                drop(chain_logs);
                drop(extension_events);
                tasks.shutdown().await;
                info!(
                    dispatched_blocks,
                    dispatched_chain_logs,
                    dispatched_extension_events,
                    uptime_secs = started.elapsed().as_secs(),
                    "graceful shutdown complete",
                );
                drop(guard);
                return (dispatched_blocks, dispatched_chain_logs);
            }
            NextEvent::StreamPanic(kind) => {
                // Reconnect tasks should loop forever.
                // Hitting `None` from `select_all` means the task
                // exited (panic or channel closed). Bail loudly.
                drop(blocks);
                drop(chain_logs);
                drop(extension_events);
                tasks.shutdown().await;
                warn!(
                    kind,
                    "reconnect task ended unexpectedly - shutting down for engine restart"
                );
                return (dispatched_blocks, dispatched_chain_logs);
            }
        }
    }
}

/// Boot cursor (clamped to head), else the invalidated tail, else
/// `resume_from`, else head; the reconnect arms are never head-clamped.
fn poller_start_block(
    boot_cursor: Option<u64>,
    resume_from: Option<u64>,
    invalidated_tail: Option<u64>,
    head: u64,
) -> u64 {
    if let Some(cursor) = boot_cursor {
        return cursor.min(head);
    }
    if let Some(tail) = invalidated_tail {
        return tail;
    }
    resume_from.unwrap_or(head)
}

/// `Some(gap)` when `now` is at least `threshold` past the last event; `None`
/// on the first event or when events arrive within `threshold`.
fn block_stream_gap_to_log(
    now: Instant,
    last_event: Option<Instant>,
    threshold: Duration,
) -> Option<Duration> {
    let last = last_event?;
    let gap = now.duration_since(last);
    (gap >= threshold).then_some(gap)
}

/// Wait for SIGINT or (on Unix) SIGTERM, whichever arrives first.
pub async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = sigterm.recv() => Ok("SIGTERM"),
            _ = sigint.recv()  => Ok("SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("ctrl-c")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_rpc_types_eth::Log;
    use alloy_transport::mock::MockResponse;
    use nexum_tasks::TaskManager;

    use crate::test_utils::rpc::{
        MockRpc, linked_block, mocked_pool, rpc_err, rpc_head, rpc_ok, test_hash,
    };
    use crate::test_utils::{BootScenario, Booted, MockTypes, mock_components};

    /// Virtual poll cadence; `start_paused` advances through it instantly.
    const POLL: Duration = Duration::from_millis(50);

    /// A zero-module supervisor over the in-process mock backends via the
    /// real boot path.
    async fn boot_mock_supervisor() -> Booted<MockTypes> {
        BootScenario::over(mock_components())
            .boot()
            .await
            .expect("boot mock supervisor")
    }

    fn pool_for(rpc: &MockRpc) -> ProviderPool {
        mocked_pool([(alloy_chains::Chain::mainnet(), rpc)], POLL)
    }

    /// A filter-matching log at `number` carrying [`linked_block`]'s hash.
    fn log_at(number: u64) -> Log {
        Log {
            block_number: Some(number),
            block_hash: Some(test_hash(number)),
            ..Default::default()
        }
    }

    /// One poller cycle: the head answer plus per-height block and log
    /// fetches; an empty height also feeds the hash-pinned fallback.
    fn cycle(head: u64, heights: &[(u64, Vec<Log>)]) -> Vec<MockResponse> {
        let mut script = vec![rpc_head(head)];
        for (number, logs) in heights {
            script.push(rpc_ok(&linked_block(*number)));
            script.push(rpc_ok(logs));
            if logs.is_empty() {
                script.push(rpc_ok(&Vec::<Log>::new()));
            }
        }
        script
    }

    /// One attempt script: head probe, optional tail probe, then the cycles.
    fn attempt(
        probe_head: u64,
        tail_probe: Option<MockResponse>,
        cycles: Vec<Vec<MockResponse>>,
    ) -> Vec<MockResponse> {
        let mut script = vec![rpc_head(probe_head)];
        script.extend(tail_probe);
        script.extend(cycles.into_iter().flatten());
        script
    }

    fn spawn_chain_log_task(
        pool: &ProviderPool,
        executor: &TaskExecutor,
        tasks: &mut TaskSet,
        initial_cursor: Option<u64>,
    ) -> TaggedChainLogStream {
        let subs = vec![ChainLogSub {
            module: "mod".into(),
            chain: alloy_chains::Chain::mainnet(),
            filter: alloy_rpc_types_eth::Filter::default(),
            cursor_key: None,
            initial_cursor,
            max_lookback: None,
        }];
        open_chain_log_streams(pool, subs, executor, tasks)
            .pop()
            .expect("one stream per subscription")
    }

    async fn recv(stream: &mut TaggedChainLogStream) -> Log {
        let (_, _, log, _) = tokio::time::timeout(Duration::from_secs(600), stream.next())
            .await
            .expect("delivery within the virtual window")
            .expect("stream alive");
        log
    }

    /// Wait for `n` ranged `eth_getLogs` fetches; returns their `fromBlock`s.
    async fn wait_for_ranged_fetches(rpc: &MockRpc, n: usize) -> Vec<u64> {
        tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let froms = rpc.log_range_froms();
                if froms.len() >= n {
                    return froms;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {n} ranged log fetches"))
    }

    /// Wait until the previous script is fully consumed; scripts end in a
    /// terminal error so the dead stream cannot misread the next phase.
    async fn drained(rpc: &MockRpc) {
        tokio::time::timeout(Duration::from_secs(600), async {
            while rpc.pending() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("script must drain");
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_with_confirmed_tail_resumes_after_it() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                vec![rpc_err("stream torn down")],
            ],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);

        let log = recv(&mut stream).await;
        assert_eq!(log.block_number, Some(90));
        drained(&rpc).await;

        rpc.push_script(attempt(
            91,
            Some(rpc_ok(&linked_block(90))),
            vec![cycle(91, &[(91, vec![log_at(91)])])],
        ));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "no retraction was synthesized");
        assert_eq!(log.block_number, Some(91));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 2).await,
            vec![90, 91],
            "first open at head, reconnect after the confirmed tail",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_with_reorged_tail_retracts_and_replays_it() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                vec![rpc_err("stream torn down")],
            ],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);
        recv(&mut stream).await;
        drained(&rpc).await;

        // Height 90 now carries a different canonical hash.
        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        let fork_log: Log = Log {
            block_number: Some(90),
            block_hash: Some(fork_hash),
            ..Default::default()
        };
        rpc.push_script(attempt(
            91,
            Some(rpc_ok(&fork_block)),
            vec![vec![
                rpc_head(90),
                rpc_ok(&fork_block),
                rpc_ok(&vec![fork_log]),
            ]],
        ));

        let retraction = recv(&mut stream).await;
        assert!(
            retraction.removed,
            "the stale delivery is retracted before any stream item",
        );
        assert_eq!(retraction.block_number, Some(90));
        assert_eq!(
            retraction.block_hash,
            Some(test_hash(90)),
            "retained logs verbatim",
        );

        let replacement = recv(&mut stream).await;
        assert!(
            !replacement.removed,
            "the canonical logs restate the height"
        );
        assert_eq!(replacement.block_number, Some(90));

        assert_eq!(
            wait_for_ranged_fetches(&rpc, 2).await,
            vec![90, 90],
            "the invalidated tail replays AT 90",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_resumes_after_empty_scanned_heights() {
        let rpc = MockRpc::new();
        let empty: Vec<(u64, Vec<Log>)> = (42..=70).map(|n| (n, Vec::new())).collect();
        rpc.push_script(attempt(
            100,
            None,
            vec![cycle(70, &empty), vec![rpc_err("stream torn down")]],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(42));

        drained(&rpc).await;
        rpc.push_script(attempt(
            100,
            None,
            vec![cycle(71, &[(71, vec![log_at(71)])])],
        ));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "nothing was retracted");
        assert_eq!(log.block_number, Some(71));

        let froms = wait_for_ranged_fetches(&rpc, 30).await;
        assert_eq!(
            froms,
            (42..=71).collect::<Vec<u64>>(),
            "the boot cursor opens AT 42; empty heights advance the resume basis to 71",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn itemless_open_reopens_at_the_same_block() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(100, None, vec![vec![rpc_err("stream torn down")]]));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let _stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        drained(&rpc).await;
        rpc.push_script(attempt(100, None, vec![cycle(0, &[(0, Vec::new())])]));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 1).await,
            vec![0],
            "an itemless open at block 0 re-opens at block 0, not at head",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn boot_cursor_survives_a_failed_open() {
        let rpc = MockRpc::new();
        rpc.push_script(vec![rpc_err("boot head fetch failed")]);
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let _stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(42));

        drained(&rpc).await;
        rpc.push_script(attempt(100, None, vec![cycle(42, &[(42, Vec::new())])]));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 1).await,
            vec![42],
            "the first successful open still starts AT the persisted cursor",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn boot_cursor_past_head_clamps_to_head() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(100, None, vec![cycle(100, &[(100, Vec::new())])]));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let _stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(150));

        assert_eq!(
            wait_for_ranged_fetches(&rpc, 1).await,
            vec![100],
            "a boot cursor past head starts at head and catches up",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn removed_batch_rolls_back_the_resume_basis() {
        let rpc = MockRpc::new();
        let mut orphaned = linked_block(91);
        orphaned.header.inner.parent_hash = B256::repeat_byte(0xdd);
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                vec![rpc_head(91), rpc_ok(&orphaned), rpc_ok(&vec![log_at(91)])],
            ],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);

        let delivered = recv(&mut stream).await;
        assert!(!delivered.removed);
        let rollback = recv(&mut stream).await;
        assert!(rollback.removed, "the poller's rollback is forwarded");
        assert_eq!(rollback.block_number, Some(90));
        drained(&rpc).await;

        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        let fork_log: Log = Log {
            block_number: Some(90),
            block_hash: Some(fork_hash),
            ..Default::default()
        };
        rpc.push_script(attempt(
            91,
            None,
            vec![vec![
                rpc_head(90),
                rpc_ok(&fork_block),
                rpc_ok(&vec![fork_log]),
            ]],
        ));
        let restated = recv(&mut stream).await;
        assert!(!restated.removed, "no duplicate synthesized retraction");
        assert_eq!(restated.block_number, Some(90));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 3).await,
            vec![90, 91, 90],
            "the rollback rolls the resume basis back to the removed height",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_tail_hash_backs_off_without_retracting() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                vec![rpc_err("stream torn down")],
            ],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);
        recv(&mut stream).await;
        drained(&rpc).await;

        // The tail probe answers with an unknown height.
        rpc.push_script(vec![
            rpc_head(91),
            rpc_ok(&Option::<alloy_rpc_types_eth::Block>::None),
        ]);
        let idle = tokio::time::timeout(Duration::from_secs(30), stream.next()).await;
        assert!(idle.is_err(), "an unconfirmed tail never delivers anything");
        assert_eq!(
            rpc.log_range_froms(),
            vec![90],
            "an unconfirmed tail never re-opens the poller",
        );

        rpc.push_script(attempt(
            91,
            Some(rpc_ok(&linked_block(90))),
            vec![cycle(91, &[(91, vec![log_at(91)])])],
        ));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "a confirmed tail resumes without retracting");
        assert_eq!(log.block_number, Some(91));
        assert_eq!(wait_for_ranged_fetches(&rpc, 2).await, vec![90, 91]);
        tasks.shutdown().await;
    }

    /// `open_block_streams` spawns one independent reconnect task per chain.
    #[tokio::test]
    async fn open_block_streams_opens_one_task_per_chain() {
        let a = MockRpc::new();
        let b = MockRpc::new();
        let pool = mocked_pool(
            [
                (alloy_chains::Chain::mainnet(), &a),
                (alloy_chains::Chain::from_id(100), &b),
            ],
            POLL,
        );
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let chains = vec![
            alloy_chains::Chain::mainnet(),
            alloy_chains::Chain::from_id(100),
        ];
        let streams = open_block_streams(&pool, &chains, &executor, &mut tasks);
        assert_eq!(streams.len(), 2, "one stream per chain");
        tasks.shutdown().await;
    }

    /// `open_chain_log_streams` spawns one reconnect task per subscription.
    #[tokio::test]
    async fn open_chain_log_streams_opens_one_task_per_subscription() {
        let rpc = MockRpc::new();
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let subs = vec![
            ChainLogSub {
                module: "mod-a".into(),
                chain: alloy_chains::Chain::mainnet(),
                filter: alloy_rpc_types_eth::Filter::default(),
                cursor_key: None,
                initial_cursor: None,
                max_lookback: None,
            },
            ChainLogSub {
                module: "mod-b".into(),
                chain: alloy_chains::Chain::mainnet(),
                filter: alloy_rpc_types_eth::Filter::default(),
                cursor_key: None,
                initial_cursor: None,
                max_lookback: None,
            },
        ];
        let streams = open_chain_log_streams(&pool, subs, &executor, &mut tasks);
        assert_eq!(streams.len(), 2, "one stream per subscription");
        tasks.shutdown().await;
    }

    /// A reconnect task whose receiver drops exits on its own with
    /// [`TaskExit::ReceiverGone`], not via abort.
    #[tokio::test(start_paused = true)]
    async fn reconnect_task_exits_receiver_gone_when_receiver_drops() {
        let rpc = MockRpc::new();
        // The open's head fetch, then one poll cycle serving block 1 - the
        // failing `tx.send` against the dropped receiver is the exit path
        // under test.
        rpc.push_script(vec![rpc_head(1), rpc_head(1), rpc_ok(&linked_block(1))]);
        let pool = pool_for(&rpc);

        let manager = TaskManager::new();
        let executor = manager.executor();
        let (tx, rx) = mpsc::channel(1);
        let handle = executor.spawn(reconnecting_block_task(
            pool,
            alloy_chains::Chain::mainnet(),
            tx,
        ));
        drop(rx);

        let exit = tokio::time::timeout(Duration::from_secs(60), handle.join())
            .await
            .expect("task must exit promptly once the receiver is gone");
        assert_eq!(
            exit,
            Some(TaskExit::ReceiverGone),
            "the task must exit naturally, not via abort (abort yields None)",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn block_subscription_reopens_after_a_failed_open() {
        let rpc = MockRpc::new();
        rpc.push_script(vec![rpc_err("node down at boot")]);
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = open_block_streams(
            &pool,
            &[alloy_chains::Chain::mainnet()],
            &executor,
            &mut tasks,
        )
        .pop()
        .expect("one stream");

        rpc.push_script(vec![rpc_head(5), rpc_head(5), rpc_ok(&linked_block(5))]);
        let header = tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                match stream.next().await.expect("stream alive") {
                    Ok((_, header)) => return header,
                    Err(_) => continue,
                }
            }
        })
        .await
        .expect("the reopened subscription delivers");
        assert_eq!(header.number, 5);
        tasks.shutdown().await;
    }

    /// No prior event yields `None`.
    #[test]
    fn block_stream_gap_to_log_returns_none_when_no_prior_event() {
        let now = Instant::now();
        assert_eq!(
            block_stream_gap_to_log(now, None, Duration::from_secs(60)),
            None,
        );
    }

    #[test]
    fn block_stream_gap_to_log_returns_none_when_under_threshold() {
        let earlier = Instant::now();
        let now = earlier + Duration::from_secs(30);
        assert_eq!(
            block_stream_gap_to_log(now, Some(earlier), Duration::from_secs(60)),
            None,
            "30s < 60s threshold -> do not log",
        );
    }

    #[test]
    fn block_stream_gap_to_log_returns_some_at_threshold_boundary() {
        let earlier = Instant::now();
        let now = earlier + Duration::from_secs(60);
        assert_eq!(
            block_stream_gap_to_log(now, Some(earlier), Duration::from_secs(60)),
            Some(Duration::from_secs(60)),
            "boundary is inclusive - exactly the threshold counts as a gap",
        );
    }

    #[test]
    fn block_stream_gap_to_log_returns_some_when_well_over_threshold() {
        let earlier = Instant::now();
        let now = earlier + Duration::from_secs(3600);
        // The 2026-06-23 soak observation: a 1h gap between the
        // `alloy_transport_ws::native` ERROR at 09:05 and the next
        // block at 10:05. This is the exact case the log line was
        // added for.
        let gap = block_stream_gap_to_log(now, Some(earlier), Duration::from_secs(60))
            .expect("1h gap is well over the 60s threshold");
        assert_eq!(gap.as_secs(), 3600);
    }

    #[test]
    fn poller_start_block_first_open_starts_at_head() {
        assert_eq!(
            poller_start_block(None, None, None, 100),
            100,
            "first open starts at head, no history replay",
        );
    }

    #[test]
    fn poller_start_block_resumes_from_the_scanned_basis() {
        assert_eq!(
            poller_start_block(None, Some(91), None, 100),
            91,
            "a re-open with a confirmed tail resumes from the scanned basis",
        );
    }

    #[test]
    fn poller_start_block_restarts_at_an_invalidated_tail() {
        assert_eq!(
            poller_start_block(None, Some(96), Some(88), 100),
            88,
            "an invalidated tail replays AT its height, below the scanned basis",
        );
    }

    #[test]
    fn poller_start_block_does_not_clamp_the_reconnect_arm_to_head() {
        // Alloy parks safely when the start is past head.
        assert_eq!(
            poller_start_block(None, Some(151), None, 100),
            151,
            "no head clamp on the reconnect arm",
        );
    }

    #[test]
    fn poller_start_block_boot_cursor_resumes_at_the_cursor() {
        assert_eq!(
            poller_start_block(Some(42), None, None, 100),
            42,
            "the persisted cursor replays AT its block, not after it",
        );
    }

    #[test]
    fn poller_start_block_boot_cursor_clamps_to_head() {
        assert_eq!(
            poller_start_block(Some(150), None, None, 100),
            100,
            "a cursor a reorg left past head starts at head and catches up",
        );
    }

    #[test]
    fn poller_start_block_treats_a_genesis_basis_as_history() {
        assert_eq!(
            poller_start_block(None, Some(0), None, 5_000),
            0,
            "an open at block 0 re-opens at block 0, not at head",
        );
    }

    /// An engine whose modules declare only `kind = "block"` (or only
    /// `kind = "chain-log"`) must not bail at boot when the other stream set
    /// is empty.
    #[tokio::test]
    async fn run_does_not_bail_when_both_stream_kinds_are_empty() {
        use std::time::{Duration, Instant};

        let mut booted = boot_mock_supervisor().await;
        let started = Instant::now();
        let shutdown = tokio::time::sleep(Duration::from_millis(50));

        crate::runtime::event_loop::run(
            &mut booted.supervisor,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            nexum_tasks::TaskSet::new(),
            shutdown,
        )
        .await;

        // If the bug were present, `run` returns ~0 ms (the empty `logs`
        // stream's first `.next()` yields `None` and the loop bails on
        // the bail-on-None arm). With the fix, `run` blocks on `shutdown`
        // for the full 50 ms.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "run returned in {elapsed:?}, expected >= ~50ms (shutdown timer)",
        );
    }

    // Verify the stream-open + run() + shutdown lifecycle end to end at the
    // supervisor boundary, without loading a real wasm module.

    /// The `biased` select drains both block and chain-log streams within one
    /// `run()` session without starving either; the returned tally shows both
    /// were consumed.
    #[tokio::test]
    async fn run_delivers_block_and_chain_log_events_without_starvation() {
        use std::time::Duration;

        use alloy_chains::Chain;
        use alloy_rpc_types_eth::Filter;

        use crate::host::provider_pool::ProviderPool;
        use crate::runtime::event_loop::{open_block_streams, open_chain_log_streams, run};
        use crate::test_utils::rpc::FakeNode;
        use nexum_tasks::{TaskManager, TaskSet};

        let mut booted = boot_mock_supervisor().await;
        let block_node = FakeNode::new();
        let log_node = FakeNode::new();
        let pool = ProviderPool::for_tests(
            [
                (Chain::mainnet(), block_node.provider()),
                (Chain::from_id(100), log_node.provider()),
            ],
            Duration::from_millis(20),
        );
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();

        // Pre-push one event of each kind before the loop starts so both mpsc
        // channels have an item for `run()` to drain on its first pass.
        block_node.push_block(alloy_rpc_types_eth::Header::default());
        log_node.push_chain_log(alloy_rpc_types_eth::Log::default());

        let block_streams = open_block_streams(&pool, &[Chain::mainnet()], &executor, &mut tasks);
        let log_subs = vec![crate::supervisor::ChainLogSub {
            module: "test-module".into(),
            chain: Chain::from_id(100),
            filter: Filter::default(),
            cursor_key: None,
            initial_cursor: None,
            max_lookback: None,
        }];
        let chain_log_streams = open_chain_log_streams(&pool, log_subs, &executor, &mut tasks);

        // The shutdown window only bounds wall time; the assertion is on the
        // tally, not on timing. 500 ms is orders of magnitude more than the
        // two channel hops need, so a miss means a broken select arm, not a
        // slow scheduler.
        let shutdown = tokio::time::sleep(Duration::from_millis(500));
        let (blocks, chain_logs) = tokio::time::timeout(
            Duration::from_secs(10),
            run(
                &mut booted.supervisor,
                block_streams,
                chain_log_streams,
                Vec::new(),
                tasks,
                shutdown,
            ),
        )
        .await
        .expect("run() must return once shutdown fires");
        assert_eq!(blocks, 1, "the queued block must be drained and dispatched");
        assert_eq!(
            chain_logs, 1,
            "the queued chain-log must be drained and dispatched",
        );
    }

    /// On the shutdown path `run()` aborts and joins every reconnect task, so
    /// none detaches and outlives the engine.
    #[tokio::test]
    async fn run_drains_reconnect_tasks_cleanly_on_shutdown() {
        use std::time::Duration;

        use alloy_chains::Chain;

        use crate::runtime::event_loop::{open_block_streams, run};
        use crate::test_utils::rpc::FakeNode;
        use nexum_tasks::{TaskManager, TaskSet};

        let mut booted = boot_mock_supervisor().await;
        let pool = FakeNode::new().pool(
            &[Chain::mainnet(), Chain::from_id(100)],
            Duration::from_millis(20),
        );
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();

        // Two subscription tasks: both must drain before `run()` returns.
        let block_streams = open_block_streams(
            &pool,
            &[Chain::mainnet(), Chain::from_id(100)],
            &executor,
            &mut tasks,
        );

        let shutdown = tokio::time::sleep(Duration::from_millis(10));
        // If the drain were absent, the spawned reconnect tasks would detach
        // and outlive the supervisor; if the drain hung, the timeout fails
        // fast instead of stalling the suite until the CI job limit.
        tokio::time::timeout(
            Duration::from_secs(10),
            run(
                &mut booted.supervisor,
                block_streams,
                vec![],
                Vec::new(),
                tasks,
                shutdown,
            ),
        )
        .await
        .expect("run() + task drain must complete promptly after shutdown");
    }
}
