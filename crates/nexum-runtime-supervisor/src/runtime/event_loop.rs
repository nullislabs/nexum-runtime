//! Open live chain event sources and dispatch their events to the supervisor
//! until shutdown. Blocks come from `eth_subscribe(newHeads)` (WS); chain-logs
//! from an `eth_getLogs` block-range poller that re-queries the reconnect gap
//! and retracts a reorged delivered tail within the revalidation depth. A
//! large gap's finalized portion is bulk-fetched in operator-declared chunks
//! before the poller opens at the reorg-window boundary.
//!
//! `open_block_streams` and `open_chain_log_streams` each spawn one
//! reconnect-aware task per event source or chain: it opens the stream,
//! pumps items to an mpsc channel, and on drop waits
//! `restart_policy::backoff_for` before reopening, resetting the backoff
//! once the stream has been healthy for `HEALTHY_WINDOW`. The tasks exit
//! with [`TaskExit::ReceiverGone`] when `run` drops the receivers, or with
//! [`TaskExit::SourceTerminal`] when the source cannot continue; their
//! handles collect into a [`TaskSet`] that `run` watches while it runs and
//! drains on shutdown. A module-owned terminal exit poisons its module; a
//! shared one ends `run` for the launcher to surface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_chains::Chain;
use alloy_primitives::B256;
use alloy_transport::TransportError;
use futures::StreamExt;
use futures::stream::{BoxStream, select_all};
use tokio::sync::mpsc;
use tracing::{error, info, instrument, warn};

use crate::bindings::nexum;
use crate::runtime::restart_policy::{backoff_for, jitter_seed};
use crate::supervisor::{EventSource, Supervisor};
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_api::{ExtensionDelivery, ExtensionSource};
use nexum_runtime_chain::{PoolError, ProviderPool};
use nexum_runtime_wasm::HostState;
use nexum_tasks::{SourceTermination, TaskExecutor, TaskExit, TaskSet};

/// Uninterrupted-event duration before the backoff counter resets to 0.
const HEALTHY_WINDOW: Duration = Duration::from_secs(60);

/// Silence between block events beyond which the next event logs a gap-closed
/// line, surfacing an alloy-internal transport reconnect that produced no
/// `source ended` event.
const BLOCK_GAP_LOG_THRESHOLD: Duration = Duration::from_secs(60);

/// Channel buffer for each reconnect task.
const RECONNECT_CHANNEL_BUF: usize = 64;

/// Block-gap size at or above which a re-open logs a large-backfill notice.
const LARGE_GAP_LOG_THRESHOLD: u64 = 1_000;

/// Bound in blocks for an invalidated-tail restart, measured below the higher
/// of the scan basis and the `max_lookback` floor.
const REVALIDATE_DEPTH: u64 = nexum_runtime_chain::MAX_REORG_DEPTH;

/// Consecutive failures on one bulk-backfill chunk before the phase is
/// abandoned to the per-block poller.
const BULK_ABANDON_ATTEMPTS: u32 = 5;

/// Minimum spacing between bulk-backfill progress log lines.
const BULK_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Wait on the task set for a terminal report behind a stream end.
const TERMINAL_REPORT_GRACE: Duration = Duration::from_secs(1);

// One spelling for the `source_kind` metric label and the span field, so an
// operator carries the alert's value straight to a log query.
pub(crate) const SOURCE_KIND_BLOCK: &str = "block";
pub(crate) const SOURCE_KIND_CHAIN_LOG: &str = "chain-log";

/// Pump tasks a stream end is charged to; `unknown` when none died, as when
/// one ended by returning an exit rather than dying.
fn dead_task_names(died: &[Arc<str>]) -> String {
    match died {
        [] => "unknown".to_owned(),
        labels => labels.join(", "),
    }
}

/// Open one reconnect-aware block-source task per chain, spawned via
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
        tasks.push(
            format!("{SOURCE_KIND_BLOCK}:{}", chain.id()),
            executor.spawn(reconnecting_block_task(pool, chain, tx)),
        );
        let tagged: TaggedBlockStream = Box::pin(receiver_stream(rx));
        streams.push(tagged);
    }
    streams
}

/// Open one reconnect-aware chain-log task per event source; see
/// [`open_block_streams`].
pub fn open_chain_log_streams(
    pool: &ProviderPool,
    sources: Vec<EventSource>,
    executor: &TaskExecutor,
    tasks: &mut TaskSet,
) -> Vec<TaggedChainLogStream> {
    let mut streams = Vec::new();
    for source in sources {
        let (tx, rx) = mpsc::channel::<TaggedChainLog>(RECONNECT_CHANNEL_BUF);
        let pool = pool.clone();
        let resume = ChainLogResume {
            // The cursor key is constant per source and cloned onto every
            // log; `Arc` keeps that clone cheap.
            cursor_key: source.cursor_key.map(Arc::from),
            initial_cursor: source.initial_cursor,
            max_lookback: source.max_lookback,
        };
        let label = format!(
            "{SOURCE_KIND_CHAIN_LOG}:{}:{}",
            source.chain.id(),
            source.module
        );
        tasks.push(
            label,
            executor.spawn(reconnecting_chain_log_task(
                pool,
                source.module,
                source.chain,
                source.filter,
                resume,
                tx,
            )),
        );
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

/// Bumps `attempt`, hands `(attempt, backoff_ms)` to the site's log line, and
/// sleeps the backoff; `seed` decorrelates the site from its co-failing peers.
async fn backoff_pause(attempt: &mut u32, seed: u64, log: impl FnOnce(u32, u64)) {
    *attempt = attempt.saturating_add(1);
    let backoff = backoff_for(*attempt, seed);
    log(*attempt, backoff.as_millis() as u64);
    tokio::time::sleep(backoff).await;
}

/// Reconnect-aware loop for one chain's block source: re-opens the
/// stream with exponential backoff after every drop or error.
///
/// The span is where every line this task emits gets its `source_kind`, at
/// `error` so no filter drops the span out from under a line it survives.
#[instrument(
    level = "error",
    name = "source",
    skip_all,
    fields(source_kind = SOURCE_KIND_BLOCK)
)]
async fn reconnecting_block_task(
    pool: ProviderPool,
    chain: Chain,
    tx: mpsc::Sender<Result<(Chain, alloy_rpc_types_eth::Header), (Chain, TransportError)>>,
) -> TaskExit {
    let chain_id = chain.id();
    let seed = jitter_seed(&format!("block-{chain_id}"));
    let mut attempt: u32 = 0;
    let mut last_event: Option<Instant> = None;
    loop {
        match pool.open_block_source(chain).await {
            Ok(mut inner) => {
                if attempt == 0 {
                    info!(chain_id, "block source open");
                } else {
                    info!(chain_id, attempt, "block source reopened");
                    metrics::counter!(
                        "nexum_runtime_source_reconnects_total",
                        "source_kind" => SOURCE_KIND_BLOCK,
                        "chain_id" => chain_id.to_string(),
                    )
                    .increment(1);
                }
                while let Some(item) = inner.next().await {
                    let now = Instant::now();
                    if attempt > 0
                        && last_event.is_some_and(|t| now.duration_since(t) >= HEALTHY_WINDOW)
                    {
                        info!(chain_id, "block source healthy - resetting backoff");
                        attempt = 0;
                    }
                    // Detect transport-layer reconnects that
                    // alloy handled internally - `inner.next().await`
                    // keeps yielding events but with a long gap. The
                    // engine's reconnect path (`source ended` -> wait
                    // backoff -> `source reopened`) does not fire
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
                            "source gap closed - first event after silence \
                             (likely an alloy-internal transport reconnect)"
                        );
                    }
                    last_event = Some(now);
                    if let Ok(header) = &item {
                        metrics::gauge!(
                            "nexum_runtime_chain_head_height",
                            "chain_id" => chain_id.to_string(),
                        )
                        .set(header.number as f64);
                    }
                    let tagged = item
                        .map(|header| (chain, header))
                        .map_err(|err| (chain, err));
                    if tx.send(tagged).await.is_err() {
                        // Receiver dropped -> engine shutting down.
                        return TaskExit::ReceiverGone;
                    }
                }
                warn!(chain_id, "block source ended (WebSocket dropped?)");
            }
            Err(err) => {
                let timed_out = matches!(err, PoolError::Timeout);
                warn!(
                    chain_id,
                    error = %err,
                    timed_out,
                    "block source open failed"
                );
            }
        }
        backoff_pause(&mut attempt, seed, |attempt, backoff_ms| {
            warn!(
                chain_id,
                attempt, backoff_ms, "reconnecting block source after backoff",
            );
        })
        .await;
    }
}

/// Per-source resume and backfill knobs for a chain-log task.
struct ChainLogResume {
    /// Durable cursor key; `Some` for a `resume` trigger.
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

/// Backfill batches sit behind the open-time head, so only a new maximum counts.
fn observe_chain_head(head_seen: &mut Option<u64>, chain_id: u64, height: u64) {
    if head_seen.is_none_or(|seen| height > seen) {
        *head_seen = Some(height);
        metrics::gauge!(
            "nexum_runtime_chain_head_height",
            "chain_id" => chain_id.to_string(),
        )
        .set(height as f64);
    }
}

/// Bulk-phase bounds `(start, handoff)`. Below the handoff every block is
/// final, so the ranged fetch needs no reorg reconciliation and the per-block
/// poller opens there to own the reorg window and the live tail.
fn bulk_backfill_bounds(start_block: u64, head: u64) -> Option<(u64, u64)> {
    let handoff = head.saturating_sub(nexum_runtime_chain::MAX_REORG_DEPTH);
    (handoff > start_block).then_some((start_block, handoff))
}

/// How a bulk phase ended.
enum BulkOutcome {
    /// Next unfetched block; equal to the handoff on completion, short of it
    /// on abandonment.
    OpenPollerAt(u64),
    /// The engine is shutting down.
    ReceiverGone,
}

/// One event source as the bulk phase borrows it from its chain-log task.
struct BulkSource<'a> {
    pool: &'a ProviderPool,
    module: &'a ModuleId,
    chain: Chain,
    filter: &'a alloy_rpc_types_eth::Filter,
    cursor_key: Option<&'a Arc<str>>,
    seed: u64,
    tx: &'a mpsc::Sender<TaggedChainLog>,
}

impl BulkSource<'_> {
    /// Fetch `from..handoff` in chunks of the operator-declared range. Each
    /// chunk's frontier goes down the same channel the poller feeds, so the
    /// durable cursor commits per chunk and an interruption resumes at the
    /// last completed chunk.
    async fn backfill(&self, from: u64, handoff: u64) -> BulkOutcome {
        let chain_id = self.chain.id();
        let chunk_blocks = match self.pool.log_range_blocks(self.chain) {
            Ok(blocks) => blocks.max(1),
            Err(err) => {
                warn!(
                    module = %self.module,
                    chain_id,
                    error = %err,
                    "bulk backfill has no declared log range - falling back to the per-block poller",
                );
                return BulkOutcome::OpenPollerAt(from);
            }
        };
        info!(
            module = %self.module,
            chain_id,
            from,
            handoff,
            chunk_blocks,
            blocks = handoff - from,
            "bulk backfill engaged over the finalized portion of the gap",
        );
        let started = Instant::now();
        let mut last_progress = started;
        let mut position = from;
        let mut attempt: u32 = 0;
        while position < handoff {
            let to = position.saturating_add(chunk_blocks - 1).min(handoff - 1);
            match self
                .pool
                .logs_in_range(self.chain, self.filter, position, to)
                .await
            {
                Ok(logs) => {
                    attempt = 0;
                    for log in logs {
                        let tagged = (
                            self.module.clone(),
                            self.chain,
                            ChainLogItem::Log(Box::new(log)),
                            self.cursor_key.cloned(),
                        );
                        if self.tx.send(tagged).await.is_err() {
                            return BulkOutcome::ReceiverGone;
                        }
                    }
                    position = to + 1;
                    // The frontier follows the chunk's logs down the same
                    // ordered channel, so it commits only after they
                    // reached the supervisor.
                    if let Some(key) = self.cursor_key {
                        let tagged = (
                            self.module.clone(),
                            self.chain,
                            ChainLogItem::Frontier(position),
                            Some(key.clone()),
                        );
                        if self.tx.send(tagged).await.is_err() {
                            return BulkOutcome::ReceiverGone;
                        }
                    }
                    let now = Instant::now();
                    if now.duration_since(last_progress) >= BULK_PROGRESS_LOG_INTERVAL {
                        last_progress = now;
                        let fetched = position - from;
                        let rate =
                            fetched as f64 / started.elapsed().as_secs_f64().max(f64::EPSILON);
                        let blocks_per_sec = format!("{rate:.2}");
                        info!(
                            module = %self.module,
                            chain_id,
                            chunk_blocks,
                            blocks_remaining = handoff - position,
                            blocks_per_sec = %blocks_per_sec,
                            "bulk backfill progressing",
                        );
                    }
                }
                Err(err) => {
                    let timed_out = matches!(err, PoolError::Timeout);
                    if attempt + 1 >= BULK_ABANDON_ATTEMPTS {
                        warn!(
                            module = %self.module,
                            chain_id,
                            from = position,
                            to,
                            error = %err,
                            timed_out,
                            blocks_remaining = handoff - position,
                            "bulk backfill abandoned after persistent chunk failures - \
                             catching up per block from here",
                        );
                        return BulkOutcome::OpenPollerAt(position);
                    }
                    backoff_pause(&mut attempt, self.seed, |attempt, backoff_ms| {
                        warn!(
                            module = %self.module,
                            chain_id,
                            from = position,
                            to,
                            error = %err,
                            timed_out,
                            attempt,
                            backoff_ms,
                            "bulk backfill chunk failed - retrying after backoff",
                        );
                    })
                    .await;
                }
            }
        }
        info!(
            module = %self.module,
            chain_id,
            handoff,
            blocks = handoff - from,
            elapsed_s = started.elapsed().as_secs(),
            "bulk backfill complete - handing off to the per-block poller",
        );
        BulkOutcome::OpenPollerAt(handoff)
    }
}

/// Poller-backed loop for one (module, chain) event source; a
/// re-open resumes past the scanned range and retracts a reorged tail
/// within the revalidation depth.
///
/// The span covers the bulk-backfill phase too, so both carry `source_kind`;
/// see [`reconnecting_block_task`] for why it sits at `error`.
#[instrument(
    level = "error",
    name = "source",
    skip_all,
    fields(source_kind = SOURCE_KIND_CHAIN_LOG)
)]
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
    let seed = jitter_seed(module.as_str()) ^ chain_id;
    let mut attempt: u32 = 0;
    let mut last_event: Option<Instant> = None;
    // One past the highest scanned height; rolled back on a removed batch.
    let mut resume_from: Option<u64> = None;
    let mut tail: Option<DeliveredTail> = None;
    // Cleared only once an open succeeds.
    let mut boot_resume: Option<u64> = initial_cursor;
    let mut head_seen: Option<u64> = None;
    loop {
        let head = match pool.head_number(chain).await {
            Ok(head) => head,
            Err(err) => {
                let timed_out = matches!(err, PoolError::Timeout);
                backoff_pause(&mut attempt, seed, |attempt, backoff_ms| {
                    warn!(
                        module = %module,
                        chain_id,
                        error = %err,
                        timed_out,
                        attempt,
                        backoff_ms,
                        "event source head fetch failed - retrying after backoff",
                    );
                })
                .await;
                continue;
            }
        };
        observe_chain_head(&mut head_seen, chain_id, head);
        // An unconfirmed tail hash is a failed open, never a retraction.
        let mut invalidated_tail: Option<u64> = None;
        if let Some(t) = &tail {
            match pool.block_by_number(chain, t.number).await {
                Ok(Some(block)) if block.header.hash == t.hash => {}
                Ok(Some(_)) => invalidated_tail = Some(t.number),
                probe => {
                    let timed_out = matches!(probe, Err(PoolError::Timeout));
                    backoff_pause(&mut attempt, seed, |attempt, backoff_ms| {
                        warn!(
                            module = %module,
                            chain_id,
                            tail_block = t.number,
                            timed_out,
                            attempt,
                            backoff_ms,
                            "event source tail hash unconfirmed - retrying after backoff",
                        );
                    })
                    .await;
                    continue;
                }
            }
        }
        let lookback_floor = max_lookback.map(|cap| head.saturating_sub(cap));
        let PollerStart {
            mut start_block,
            restates_tail,
        } = poller_start_block(
            boot_resume,
            resume_from,
            invalidated_tail,
            head,
            lookback_floor,
        );
        if let Some(tail_block) = invalidated_tail
            && !restates_tail
        {
            warn!(
                module = %module,
                chain_id,
                tail_block,
                start_block,
                "event source tail reorged deeper than the revalidation bound - terminal",
            );
            return TaskExit::SourceTerminal(SourceTermination {
                module: Some(module.to_string()),
                chain_id,
                reason: format!(
                    "delivered tail at block {tail_block} reorged deeper than the \
                     revalidation bound ({REVALIDATE_DEPTH} blocks); no rescan \
                     from {start_block} can restate its logs"
                ),
            });
        }
        // Opt-in bound: `max_lookback` caps how far back a resume
        // trigger backfills. The default (`None`) backfills fully; a
        // set cap clamps the start up to `head - cap` and surfaces the
        // dropped oldest blocks. A pending retraction is exempt: nothing
        // would restate the retracted logs if the clamp raised the start
        // above them.
        if let Some(floor) = lookback_floor
            && !restates_tail
            && start_block < floor
        {
            warn!(
                module = %module,
                chain_id,
                skipped_from = start_block,
                skipped_to = floor,
                "event source gap exceeds max_lookback - skipping the oldest missed blocks",
            );
            start_block = floor;
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
                "event source backfilling a large gap"
            );
        }
        // A pending retraction stays on the per-block path: the bulk phase
        // would restate the tail's height before the retraction below is
        // emitted.
        if invalidated_tail.is_none() {
            // Blocks produced during a long pass re-enter the bulk phase
            // against a re-read head, so the poller inherits only a
            // sub-threshold residue.
            let mut bulk_head = head;
            while let Some((from, handoff)) = bulk_backfill_bounds(start_block, bulk_head) {
                let source = BulkSource {
                    pool: &pool,
                    module: &module,
                    chain,
                    filter: &filter,
                    cursor_key: cursor_key.as_ref(),
                    seed,
                    tx: &tx,
                };
                match source.backfill(from, handoff).await {
                    BulkOutcome::OpenPollerAt(next) if next > from => {
                        boot_resume = None;
                        resume_from = Some(next);
                        start_block = next;
                        if next < handoff {
                            break;
                        }
                    }
                    BulkOutcome::OpenPollerAt(_) => break,
                    BulkOutcome::ReceiverGone => return TaskExit::ReceiverGone,
                }
                match pool.head_number(chain).await {
                    Ok(new_head) => {
                        observe_chain_head(&mut head_seen, chain_id, new_head);
                        bulk_head = new_head;
                    }
                    // The poller's own open path retries the head.
                    Err(_) => break,
                }
            }
        }
        match pool.open_event_source(chain, filter.clone(), start_block) {
            Ok(mut inner) => {
                if attempt == 0 {
                    info!(
                        module = %module,
                        chain_id,
                        start_block,
                        "event source open"
                    );
                } else {
                    info!(
                        module = %module,
                        chain_id,
                        attempt,
                        start_block,
                        "event source reopened"
                    );
                    metrics::counter!(
                        "nexum_runtime_source_reconnects_total",
                        "source_kind" => SOURCE_KIND_CHAIN_LOG,
                        "chain_id" => chain_id.to_string(),
                        "module" => module.clone(),
                    )
                    .increment(1);
                }
                // An itemless open re-opens at the same block.
                boot_resume = None;
                resume_from = Some(start_block);
                // Retract before pumping: the terminal return above leaves
                // only the within-bound case, whose scan restates the tail.
                if invalidated_tail.is_some()
                    && let Some(t) = tail.take()
                {
                    warn!(
                        module = %module,
                        chain_id,
                        tail_block = t.number,
                        "event source tail reorged while disconnected - retracting its logs",
                    );
                    for mut log in t.logs {
                        log.removed = true;
                        let tagged = (
                            module.clone(),
                            chain,
                            ChainLogItem::Log(Box::new(log)),
                            cursor_key.clone(),
                        );
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
                            "event source healthy - resetting backoff"
                        );
                        attempt = 0;
                    }
                    last_event = Some(now);
                    match item {
                        // Each log arrives with `removed` already stamped.
                        Ok(batch) => {
                            observe_chain_head(&mut head_seen, chain_id, batch.number);
                            for log in &batch.logs {
                                let tagged = (
                                    module.clone(),
                                    chain,
                                    ChainLogItem::Log(Box::new(log.clone())),
                                    cursor_key.clone(),
                                );
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
                                "event source error - reopening"
                            );
                            break;
                        }
                    }
                }
                warn!(
                    module = %module,
                    chain_id,
                    "event source ended - reopening"
                );
            }
            Err(err) => {
                warn!(
                    module = %module,
                    chain_id,
                    error = %err,
                    "event source open failed"
                );
            }
        }
        backoff_pause(&mut attempt, seed, |attempt, backoff_ms| {
            warn!(
                module = %module,
                chain_id,
                attempt,
                backoff_ms,
                "reconnecting event source after backoff",
            );
        })
        .await;
    }
}

/// Block headers tagged with the chain they came from. The error side
/// carries the chain too, so a failing stream names itself.
pub type TaggedBlockStream = std::pin::Pin<
    Box<
        dyn futures::Stream<
                Item = Result<(Chain, alloy_rpc_types_eth::Header), (Chain, TransportError)>,
            > + Send,
    >,
>;
/// One chain-log channel item.
pub enum ChainLogItem {
    /// A log to dispatch, `removed` already stamped.
    Log(Box<alloy_rpc_types_eth::Log>),
    /// First block past a completed bulk chunk; commits the resume cursor
    /// without a dispatch.
    Frontier(u64),
}

/// `(module, chain, item, cursor_key)`; `cursor_key` is `Some` for `resume`.
pub type TaggedChainLog = (ModuleId, Chain, ChainLogItem, Option<Arc<str>>);
/// Stream of [`TaggedChainLog`], merged across every open event source.
pub type TaggedChainLogStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = TaggedChainLog> + Send>>;

/// Why [`run`] returned.
#[derive(Debug)]
pub enum RunEnd {
    /// The `shutdown` future resolved; a clean stop.
    Shutdown,
    /// A non-empty stream set ended without a terminal report, so a pump
    /// panicked or was aborted; the launcher surfaces it as a non-zero exit.
    StreamEnded,
    /// Every event source ended terminally and no block or extension stream
    /// is declared, so no trigger can fire again; a clean stop.
    NothingLive,
    /// A shared source reported a terminal condition; the launcher surfaces
    /// it as a non-zero exit.
    SourceTerminal(SourceTermination),
}

/// [`run`]'s dispatch tally and why it stopped.
#[derive(Debug)]
pub struct RunOutcome {
    /// Blocks dispatched over the run.
    pub dispatched_blocks: u64,
    /// Chain-log events dispatched over the run.
    pub dispatched_events: u64,
    /// Why the loop returned.
    pub end: RunEnd,
}

/// Drive the supervisor with triggers until `shutdown` resolves, watching
/// `tasks` for a source exit in the meantime.
///
/// `shutdown` is observed only between guest calls, never
/// mid-`call_on_trigger`: here between triggers, and through the
/// supervisor's stop probe between the per-module calls of one trigger. The
/// in-flight call finishes before the loop exits; the guard `shutdown`
/// yields is held until return, so the drain covers that call and its
/// cursor commit.
pub async fn run<T: RuntimeTypes<State = HostState<T>>, G>(
    supervisor: &mut Supervisor<T>,
    block_streams: Vec<TaggedBlockStream>,
    chain_log_streams: Vec<TaggedChainLogStream>,
    extension_streams: Vec<ExtensionSource>,
    mut tasks: TaskSet,
    shutdown: impl std::future::Future<Output = G> + Send,
) -> RunOutcome {
    let chain_log_sources = chain_log_streams.len();
    let has_blocks = !block_streams.is_empty();
    let has_extensions = !extension_streams.is_empty();
    // `select_all` over an empty Vec yields `None` immediately, which
    // would trip the "stream ended -> shut down" arm below before the
    // first block / chain-log ever flows. Engine configs that declare
    // only one trigger kind (e.g. all modules use `[[trigger]] on
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
    let mut extension_deliveries: BoxStream<'_, _> = if extension_streams.is_empty() {
        futures::stream::pending().boxed()
    } else {
        select_all(extension_streams).boxed()
    };
    let mut shutdown = Box::pin(shutdown);
    let mut dispatched_blocks: u64 = 0;
    let mut dispatched_events: u64 = 0;
    let mut dispatched_extension_triggers: u64 = 0;
    // Chain-log sources are the module-owned ones; once every one of them
    // has reported terminally, the merged stream's end is expected.
    let mut terminal_chain_log_exits: usize = 0;
    let started = Instant::now();
    loop {
        // Phase 1: pick the next trigger OR observe shutdown. The
        // dispatch itself happens in phase 2 (outside the select)
        // so an in-flight wasmtime call never gets cancelled by a
        // shutdown signal arriving mid-dispatch.
        enum NextTrigger<G> {
            Block(nexum::host::types::Block),
            // The alloy `Log` is boxed so the `Chain` tag does not push
            // the enum past the large-variant lint threshold.
            Event(
                ModuleId,
                Chain,
                Box<alloy_rpc_types_eth::Log>,
                Option<Arc<str>>,
            ),
            CursorFrontier(ModuleId, Arc<str>, u64),
            Extension(ExtensionDelivery),
            // Carries the drain guard `shutdown` yielded.
            Shutdown(G),
            SourceExit(TaskExit),
            StreamEnd(&'static str),
        }
        let next = tokio::select! {
            biased;
            guard = &mut shutdown => NextTrigger::Shutdown(guard),
            exit = tasks.join_next() => NextTrigger::SourceExit(exit),
            next = blocks.next() => match next {
                Some(Ok((chain, header))) => NextTrigger::Block(nexum::host::types::Block {
                    chain_id: chain.id(),
                    number: header.number,
                    hash: header.hash.as_slice().to_vec(),
                    timestamp: header.timestamp.saturating_mul(1000),
                }),
                Some(Err((chain, err))) => {
                    warn!(
                        chain_id = chain.id(),
                        source_kind = SOURCE_KIND_BLOCK,
                        error = %err,
                        "block source error - continuing"
                    );
                    continue;
                }
                None => NextTrigger::StreamEnd(SOURCE_KIND_BLOCK),
            },
            next = chain_logs.next() => match next {
                Some((module, chain, ChainLogItem::Log(log), cursor_key)) => {
                    NextTrigger::Event(module, chain, log, cursor_key)
                }
                Some((module, _, ChainLogItem::Frontier(frontier), Some(key))) => {
                    NextTrigger::CursorFrontier(module, key, frontier)
                }
                // A frontier without a cursor key has nothing to commit.
                Some((_, _, ChainLogItem::Frontier(_), None)) => continue,
                None => NextTrigger::StreamEnd(SOURCE_KIND_CHAIN_LOG),
            },
            next = extension_deliveries.next() => match next {
                Some(delivery) => NextTrigger::Extension(delivery),
                // Extension source tasks loop forever; `None` means one exited.
                None => NextTrigger::StreamEnd("extension"),
            },
        };

        match next {
            NextTrigger::Block(block) => {
                supervisor.dispatch_block(block).await;
                dispatched_blocks += 1;
            }
            NextTrigger::Event(module, chain, log, cursor_key) => {
                supervisor
                    .dispatch_event(&module, chain, *log, cursor_key.as_deref())
                    .await;
                dispatched_events += 1;
            }
            NextTrigger::CursorFrontier(module, key, frontier) => {
                supervisor.commit_chain_log_frontier(&module, &key, frontier);
            }
            NextTrigger::Extension(delivery) => {
                supervisor.dispatch_extension_trigger(delivery).await;
                dispatched_extension_triggers += 1;
            }
            NextTrigger::SourceExit(TaskExit::SourceTerminal(term)) => match term.module {
                Some(ref name) => {
                    terminal_chain_log_exits += 1;
                    supervisor.poison_source(name, term.chain_id, &term.reason);
                }
                None => {
                    drop(blocks);
                    drop(chain_logs);
                    drop(extension_deliveries);
                    tasks.shutdown().await;
                    error!(
                        chain_id = term.chain_id,
                        reason = %term.reason,
                        "shared source terminal - engine exiting",
                    );
                    return RunOutcome {
                        dispatched_blocks,
                        dispatched_events,
                        end: RunEnd::SourceTerminal(term),
                    };
                }
            },
            // A pump reports `ReceiverGone` only once its receiver dropped,
            // which this loop does after it stops selecting.
            NextTrigger::SourceExit(TaskExit::ReceiverGone) => {}
            NextTrigger::Shutdown(guard) => {
                // Drop the stream-end receivers so the reconnect
                // tasks observe a closed channel and exit. Then drain
                // the task set so the engine genuinely sees the tasks
                // finish before returning.
                drop(blocks);
                drop(chain_logs);
                drop(extension_deliveries);
                tasks.shutdown().await;
                info!(
                    dispatched_blocks,
                    dispatched_events,
                    dispatched_extension_triggers,
                    uptime_secs = started.elapsed().as_secs(),
                    "graceful shutdown complete",
                );
                drop(guard);
                return RunOutcome {
                    dispatched_blocks,
                    dispatched_events,
                    end: RunEnd::Shutdown,
                };
            }
            NextTrigger::StreamEnd(stream_kind) => {
                // A finishing task drops its stream sender before its join
                // handle resolves, so this end can arrive ahead of the
                // terminal report behind it: absorb the set's imminent exits
                // before ruling on the end.
                let mut shared: Option<SourceTermination> = None;
                let mut accounted = false;
                loop {
                    if stream_kind == SOURCE_KIND_CHAIN_LOG
                        && terminal_chain_log_exits == chain_log_sources
                    {
                        accounted = true;
                        break;
                    }
                    match tokio::time::timeout(TERMINAL_REPORT_GRACE, tasks.join_next()).await {
                        Ok(TaskExit::SourceTerminal(term)) => match term.module {
                            Some(ref name) => {
                                terminal_chain_log_exits += 1;
                                supervisor.poison_source(name, term.chain_id, &term.reason);
                            }
                            None => {
                                shared = Some(term);
                                break;
                            }
                        },
                        Ok(TaskExit::ReceiverGone) => {}
                        Err(_) => break,
                    }
                }
                if let Some(term) = shared {
                    drop(blocks);
                    drop(chain_logs);
                    drop(extension_deliveries);
                    tasks.shutdown().await;
                    error!(
                        chain_id = term.chain_id,
                        reason = %term.reason,
                        "shared source terminal - engine exiting",
                    );
                    return RunOutcome {
                        dispatched_blocks,
                        dispatched_events,
                        end: RunEnd::SourceTerminal(term),
                    };
                }
                if accounted && (has_blocks || has_extensions) {
                    // Park the exhausted merge so its `None` is not
                    // re-selected.
                    chain_logs = futures::stream::pending().boxed();
                } else if accounted {
                    drop(blocks);
                    drop(chain_logs);
                    drop(extension_deliveries);
                    tasks.shutdown().await;
                    warn!(
                        "every event source is terminal and no other trigger kind \
                         is declared - engine has nothing left to run; exiting"
                    );
                    return RunOutcome {
                        dispatched_blocks,
                        dispatched_events,
                        end: RunEnd::NothingLive,
                    };
                } else {
                    // Reconnect tasks should loop forever, so an end the set
                    // does not account for means one exited without a report
                    // (panic or abort).
                    drop(blocks);
                    drop(chain_logs);
                    drop(extension_deliveries);
                    // The grace loop above already discarded the dead handle,
                    // so read its label before `shutdown` consumes the set.
                    let died = dead_task_names(tasks.died());
                    tasks.shutdown().await;
                    warn!(
                        source_kind = stream_kind,
                        task = %died,
                        "reconnect task ended unexpectedly - shutting down for engine restart"
                    );
                    return RunOutcome {
                        dispatched_blocks,
                        dispatched_events,
                        end: RunEnd::StreamEnded,
                    };
                }
            }
        }
    }
}

/// Where a poller (re-)open starts, and whether that scan covers the
/// delivered tail, so a retraction of it is followed by a restatement.
struct PollerStart {
    start_block: u64,
    restates_tail: bool,
}

/// Boot cursor (clamped to head), else the invalidated tail bounded to
/// [`REVALIDATE_DEPTH`], else `resume_from`, else head; the reconnect arms
/// are never head-clamped.
///
/// A tail below the bound cannot be restated at any bounded start; the
/// caller reports it as terminal for the source's module.
fn poller_start_block(
    boot_cursor: Option<u64>,
    resume_from: Option<u64>,
    invalidated_tail: Option<u64>,
    head: u64,
    lookback_floor: Option<u64>,
) -> PollerStart {
    if let Some(cursor) = boot_cursor {
        return PollerStart {
            start_block: cursor.min(head),
            restates_tail: false,
        };
    }
    let basis = resume_from.unwrap_or(head);
    if let Some(tail) = invalidated_tail {
        let bound = basis
            .max(lookback_floor.unwrap_or(0))
            .saturating_sub(REVALIDATE_DEPTH);
        return if tail >= bound {
            PollerStart {
                start_block: tail,
                restates_tail: true,
            }
        } else {
            PollerStart {
                start_block: bound,
                restates_tail: false,
            }
        };
    }
    PollerStart {
        start_block: basis,
        restates_tail: false,
    }
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
pub async fn wait_for_os_signal() -> std::io::Result<&'static str> {
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
    use tracing::instrument::WithSubscriber;

    use crate::test_utils::{BootScenario, Booted, MockTypes, mock_components};
    use crate::test_utils::{
        JsonLogs, MockRpc, json_collector, linked_block, mocked_pool, rpc_err, rpc_head, rpc_ok,
        test_hash,
    };

    /// Virtual poll cadence; `start_paused` advances through it instantly.
    const POLL: Duration = Duration::from_millis(50);

    /// A zero-module supervisor booted through the real boot path.
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
        spawn_chain_log_task_with_lookback(pool, executor, tasks, initial_cursor, None)
    }

    fn spawn_chain_log_task_with_lookback(
        pool: &ProviderPool,
        executor: &TaskExecutor,
        tasks: &mut TaskSet,
        initial_cursor: Option<u64>,
        max_lookback: Option<u64>,
    ) -> TaggedChainLogStream {
        let sources = vec![EventSource {
            module: ModuleId::parse("mod").expect("valid module name"),
            chain: alloy_chains::Chain::mainnet(),
            filter: alloy_rpc_types_eth::Filter::default(),
            cursor_key: None,
            initial_cursor,
            max_lookback,
        }];
        open_chain_log_streams(pool, sources, executor, tasks)
            .pop()
            .expect("one stream per source")
    }

    /// Like [`spawn_chain_log_task`], with a cursor key so the bulk phase
    /// emits frontier items.
    fn spawn_keyed_chain_log_task(
        pool: &ProviderPool,
        executor: &TaskExecutor,
        tasks: &mut TaskSet,
        initial_cursor: Option<u64>,
    ) -> TaggedChainLogStream {
        let sources = vec![EventSource {
            module: ModuleId::parse("mod").expect("valid module name"),
            chain: alloy_chains::Chain::mainnet(),
            filter: alloy_rpc_types_eth::Filter::default(),
            cursor_key: Some("cursor-key".to_owned()),
            initial_cursor,
            max_lookback: None,
        }];
        open_chain_log_streams(pool, sources, executor, tasks)
            .pop()
            .expect("one stream per source")
    }

    async fn recv(stream: &mut TaggedChainLogStream) -> Log {
        match recv_item(stream).await {
            ChainLogItem::Log(log) => *log,
            ChainLogItem::Frontier(block) => panic!("expected a log, got the frontier {block}"),
        }
    }

    async fn recv_item(stream: &mut TaggedChainLogStream) -> ChainLogItem {
        let (_, _, item, _) = tokio::time::timeout(Duration::from_secs(600), stream.next())
            .await
            .expect("delivery within the virtual window")
            .expect("stream alive");
        item
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

    /// `toBlock` of every captured ranged `eth_getLogs`, in call order.
    fn log_range_tos(rpc: &MockRpc) -> Vec<u64> {
        rpc.captured()
            .iter()
            .filter(|req| req.method == "eth_getLogs")
            .filter_map(|req| {
                let to = req.params.get(0)?.get("toBlock")?.as_str()?;
                u64::from_str_radix(to.trim_start_matches("0x"), 16).ok()
            })
            .collect()
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
    async fn max_lookback_yields_to_a_pending_retraction() {
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
        // A cap of 2 puts the clamp floor at 93 on the reconnect, above the
        // delivered tail at 90.
        let mut stream =
            spawn_chain_log_task_with_lookback(&pool, &executor, &mut tasks, None, Some(2));
        recv(&mut stream).await;
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
            95,
            Some(rpc_ok(&fork_block)),
            vec![vec![
                rpc_head(90),
                rpc_ok(&fork_block),
                rpc_ok(&vec![fork_log]),
            ]],
        ));

        let retraction = recv(&mut stream).await;
        assert!(retraction.removed, "the stale delivery is retracted");
        assert_eq!(retraction.block_number, Some(90));
        let replacement = recv(&mut stream).await;
        assert!(
            !replacement.removed,
            "the guest that received the retraction receives the restatement",
        );
        assert_eq!(replacement.block_number, Some(90));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 2).await,
            vec![90, 90],
            "the clamp yields: the restart stays AT the invalidated tail, below the floor 93",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn tail_far_below_the_lookback_floor_ends_the_task_with_a_terminal_report() {
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
        let mut stream =
            spawn_chain_log_task_with_lookback(&pool, &executor, &mut tasks, None, Some(2));
        recv(&mut stream).await;
        drained(&rpc).await;

        // The floor at 998 leaves the tail at 90 more than REVALIDATE_DEPTH
        // below it, so the bound hangs off the floor and holds.
        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        rpc.push_script(attempt(1_000, Some(rpc_ok(&fork_block)), Vec::new()));

        let exit = tokio::time::timeout(Duration::from_secs(600), tasks.join_next())
            .await
            .expect("the task ends instead of reopening");
        assert!(
            matches!(
                &exit,
                TaskExit::SourceTerminal(term) if term.module.as_deref() == Some("mod"),
            ),
            "a tail unrestatable under the floor-relative bound is terminal too: {exit:?}",
        );
        assert!(
            stream.next().await.is_none(),
            "no interim retraction is delivered; the stream ends with the task",
        );
        assert_eq!(
            rpc.log_range_froms(),
            vec![90],
            "the reconnect opens no poller and fetches nothing further",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tail_deeper_than_the_bound_ends_the_task_with_a_terminal_report() {
        let rpc = MockRpc::new();
        // Empty heights push the scan basis to 156, leaving the delivered
        // tail at 90 more than REVALIDATE_DEPTH below it.
        let empty: Vec<(u64, Vec<Log>)> = (91..=155).map(|n| (n, Vec::new())).collect();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                cycle(155, &empty),
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

        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        rpc.push_script(attempt(156, Some(rpc_ok(&fork_block)), Vec::new()));

        let exit = tokio::time::timeout(Duration::from_secs(600), tasks.join_next())
            .await
            .expect("the task ends instead of reopening");
        let TaskExit::SourceTerminal(term) = exit else {
            panic!("expected a terminal exit, got {exit:?}");
        };
        assert_eq!(term.module.as_deref(), Some("mod"));
        assert_eq!(term.chain_id, 1);
        assert!(
            term.reason.contains("revalidation bound"),
            "the reason names the bound: {}",
            term.reason,
        );
        assert!(
            stream.next().await.is_none(),
            "no interim retraction is delivered; the stream ends with the task",
        );
        assert_eq!(
            rpc.log_range_froms(),
            (90..=155).collect::<Vec<u64>>(),
            "the reconnect opens no poller and fetches nothing further",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn max_lookback_still_clamps_without_a_pending_retraction() {
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
        let mut stream =
            spawn_chain_log_task_with_lookback(&pool, &executor, &mut tasks, None, Some(5));
        recv(&mut stream).await;
        drained(&rpc).await;

        // The tail at 90 confirms, so the clamp raises the resume from 91 to 95.
        rpc.push_script(attempt(
            100,
            Some(rpc_ok(&linked_block(90))),
            vec![cycle(95, &[(95, vec![log_at(95)])])],
        ));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "nothing was retracted");
        assert_eq!(log.block_number, Some(95));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 2).await,
            vec![90, 95],
            "with no retraction pending the clamp still raises the start to head - cap",
        );
        tasks.shutdown().await;
    }

    #[test]
    fn bulk_backfill_bounds_covers_every_finalized_block() {
        // head 2_064 puts the handoff at 2_000, and the bulk phase takes
        // everything below it however little that is.
        assert_eq!(bulk_backfill_bounds(1_000, 2_064), Some((1_000, 2_000)));
        assert_eq!(bulk_backfill_bounds(1_999, 2_064), Some((1_999, 2_000)));
        assert_eq!(
            bulk_backfill_bounds(2_000, 2_064),
            None,
            "a gap wholly inside the reorg window stays on the per-block poller",
        );
    }

    #[test]
    fn bulk_backfill_bounds_ignores_a_head_inside_the_reorg_window() {
        assert_eq!(bulk_backfill_bounds(0, 63), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_large_gap_bulk_backfills_in_chunks_and_hands_off_to_the_poller() {
        let rpc = MockRpc::new();
        // Boot cursor 0 against head 2_064: handoff 2_000, three 800-block
        // chunks, then the per-block poller from the handoff.
        let mut script = vec![rpc_head(2_064)];
        script.push(rpc_ok(&vec![log_at(10)]));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_ok(&vec![log_at(1_900)]));
        // The post-pass head re-read finds no further finalized gap.
        script.push(rpc_head(2_064));
        script.extend(cycle(2_064, &[(2_000, vec![log_at(2_000)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert_eq!(recv(&mut stream).await.block_number, Some(10));
        assert_eq!(recv(&mut stream).await.block_number, Some(1_900));
        assert_eq!(
            recv(&mut stream).await.block_number,
            Some(2_000),
            "the poller's first delivery follows the bulk logs in order",
        );
        // The script is exhausted once 2_000 delivers, so the poller's
        // further (failing) fetches may trail the asserted prefix.
        let froms = wait_for_ranged_fetches(&rpc, 4).await;
        assert_eq!(
            &froms[..4],
            [0, 800, 1_600, 2_000],
            "three declared-range chunks, then the poller opens at head - MAX_REORG_DEPTH",
        );
        assert_eq!(
            &log_range_tos(&rpc)[..4],
            [799, 1_599, 1_999, 2_000],
            "the chunks stop below the handoff; the poller owns the reorg window",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_head_that_advances_during_the_bulk_phase_is_backfilled_too() {
        let rpc = MockRpc::new();
        // Head 1_064 puts the first handoff at 1_000, covered by one chunk.
        // By the re-read the chain has moved to 1_864, so a second pass takes
        // 1_000 to 1_800 before the poller opens.
        let mut script = vec![rpc_head(1_064)];
        script.push(rpc_ok(&vec![log_at(10)]));
        script.push(rpc_head(1_864));
        script.push(rpc_ok(&vec![log_at(1_500)]));
        script.push(rpc_head(1_864));
        script.extend(cycle(1_864, &[(1_800, vec![log_at(1_800)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(1_000);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert_eq!(recv(&mut stream).await.block_number, Some(10));
        assert_eq!(
            recv(&mut stream).await.block_number,
            Some(1_500),
            "the drift that appeared during the first pass is bulk-fetched, not walked per block",
        );
        assert_eq!(recv(&mut stream).await.block_number, Some(1_800));
        let froms = wait_for_ranged_fetches(&rpc, 3).await;
        assert_eq!(
            &froms[..3],
            [0, 1_000, 1_800],
            "two bulk passes against a moving head, then the poller at the final handoff",
        );
        assert_eq!(
            &log_range_tos(&rpc)[..3],
            [999, 1_799, 1_800],
            "the second pass stops below the handoff the re-read produced",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn bulk_chunks_deliver_their_frontier_through_the_log_channel() {
        let rpc = MockRpc::new();
        let mut script = vec![rpc_head(2_064)];
        script.push(rpc_ok(&vec![log_at(10)]));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_ok(&vec![log_at(1_900)]));
        script.push(rpc_head(2_064));
        script.extend(cycle(2_064, &[(2_000, vec![log_at(2_000)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_keyed_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert!(matches!(
            recv_item(&mut stream).await,
            ChainLogItem::Log(log) if log.block_number == Some(10),
        ));
        assert!(
            matches!(recv_item(&mut stream).await, ChainLogItem::Frontier(800)),
            "each chunk closes with its frontier, behind the chunk's logs",
        );
        assert!(
            matches!(recv_item(&mut stream).await, ChainLogItem::Frontier(1_600)),
            "a log-free chunk still moves the frontier",
        );
        assert!(matches!(
            recv_item(&mut stream).await,
            ChainLogItem::Log(log) if log.block_number == Some(1_900),
        ));
        assert!(
            matches!(recv_item(&mut stream).await, ChainLogItem::Frontier(2_000)),
            "the last chunk's frontier is the handoff",
        );
        assert!(matches!(
            recv_item(&mut stream).await,
            ChainLogItem::Log(log) if log.block_number == Some(2_000),
        ));
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_boot_cursor_mid_backfill_resumes_at_the_last_committed_chunk() {
        let rpc = MockRpc::new();
        // A restart left the cursor at the 800 chunk boundary; head 2_064
        // leaves two chunks and then the poller at the handoff.
        let mut script = vec![rpc_head(2_064)];
        script.push(rpc_ok(&vec![log_at(900)]));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_head(2_064));
        script.extend(cycle(2_064, &[(2_000, vec![log_at(2_000)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(800));

        assert_eq!(recv(&mut stream).await.block_number, Some(900));
        assert_eq!(recv(&mut stream).await.block_number, Some(2_000));
        let froms = wait_for_ranged_fetches(&rpc, 3).await;
        assert_eq!(
            &froms[..3],
            [800, 1_600, 2_000],
            "no block below the committed chunk boundary is refetched",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn blocks_produced_during_the_bulk_phase_backfill_in_bulk_too() {
        let rpc = MockRpc::new();
        let mut script = vec![rpc_head(2_064)];
        script.push(rpc_ok(&vec![log_at(10)]));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_ok(&Vec::<Log>::new()));
        // The head moved a bulk-sized gap ahead during the first pass, so a
        // second pass covers [2_000, 3_063] before the poller opens.
        script.push(rpc_head(3_128));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_ok(&vec![log_at(3_000)]));
        script.push(rpc_head(3_128));
        script.extend(cycle(3_128, &[(3_064, vec![log_at(3_064)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert_eq!(recv(&mut stream).await.block_number, Some(10));
        assert_eq!(recv(&mut stream).await.block_number, Some(3_000));
        assert_eq!(recv(&mut stream).await.block_number, Some(3_064));
        let froms = wait_for_ranged_fetches(&rpc, 6).await;
        assert_eq!(
            &froms[..6],
            [0, 800, 1_600, 2_000, 2_800, 3_064],
            "the second pass bulk-fetches the blocks the first pass left behind",
        );
        assert_eq!(
            &log_range_tos(&rpc)[..6],
            [799, 1_599, 1_999, 2_799, 3_063, 3_064],
            "both passes stop below their handoff",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_persistently_failing_chunk_abandons_the_bulk_phase_to_the_poller() {
        let rpc = MockRpc::new();
        let mut script = vec![rpc_head(2_064)];
        // The first chunk delivers; every retry of the second fails.
        script.push(rpc_ok(&vec![log_at(10)]));
        script.extend((0..5).map(|_| rpc_err("range refused")));
        script.extend(cycle(2_064, &[(800, vec![log_at(800)])]));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert_eq!(recv(&mut stream).await.block_number, Some(10));
        assert_eq!(
            recv(&mut stream).await.block_number,
            Some(800),
            "the poller opens at the abandoned position, not at the handoff",
        );
        let froms = wait_for_ranged_fetches(&rpc, 7).await;
        assert_eq!(
            &froms[..7],
            [0, 800, 800, 800, 800, 800, 800],
            "five attempts on the failed chunk, then per-block from there",
        );
        assert_eq!(
            &log_range_tos(&rpc)[..7],
            [799, 1_599, 1_599, 1_599, 1_599, 1_599, 800],
            "every retry repeats the chunk unchanged",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_after_the_bulk_phase_does_not_refetch_bulk_chunks() {
        let rpc = MockRpc::new();
        let mut script = vec![rpc_head(2_064)];
        script.push(rpc_ok(&vec![log_at(10)]));
        script.push(rpc_ok(&Vec::<Log>::new()));
        script.push(rpc_ok(&Vec::<Log>::new()));
        // The post-pass head re-read finds no further finalized gap.
        script.push(rpc_head(2_064));
        // The poller's first head fetch fails, forcing a reconnect.
        script.push(rpc_err("stream torn down"));
        rpc.push_script(script);
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(0));

        assert_eq!(recv(&mut stream).await.block_number, Some(10));
        drained(&rpc).await;

        rpc.push_script(attempt(
            2_064,
            None,
            vec![cycle(2_064, &[(2_000, vec![log_at(2_000)])])],
        ));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "nothing was retracted");
        assert_eq!(log.block_number, Some(2_000));
        let froms = wait_for_ranged_fetches(&rpc, 4).await;
        assert_eq!(
            &froms[..4],
            [0, 800, 1_600, 2_000],
            "the reconnect resumes at the handoff; no bulk chunk is refetched",
        );
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_retraction_keeps_the_gap_on_the_per_block_path() {
        let rpc = MockRpc::new();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                vec![rpc_err("stream torn down")],
            ],
        ));
        let pool = pool_for(&rpc).with_log_range_blocks(800);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);
        recv(&mut stream).await;
        drained(&rpc).await;

        // Head 5_000 leaves a bulk-sized gap, but height 90 reorged while
        // disconnected, so the retraction and restatement must pair up on the
        // per-block path.
        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        let fork_log: Log = Log {
            block_number: Some(90),
            block_hash: Some(fork_hash),
            ..Default::default()
        };
        rpc.push_script(attempt(
            5_000,
            Some(rpc_ok(&fork_block)),
            vec![vec![
                rpc_head(90),
                rpc_ok(&fork_block),
                rpc_ok(&vec![fork_log]),
            ]],
        ));

        let retraction = recv(&mut stream).await;
        assert!(retraction.removed, "the stale delivery is retracted");
        assert_eq!(retraction.block_number, Some(90));
        let replacement = recv(&mut stream).await;
        assert!(!replacement.removed);
        assert_eq!(replacement.block_number, Some(90));
        assert_eq!(
            wait_for_ranged_fetches(&rpc, 2).await,
            vec![90, 90],
            "the invalidated tail replays AT 90 with no bulk phase in front of it",
        );
        assert_eq!(
            log_range_tos(&rpc),
            vec![90, 90],
            "no chunk-wide fetch ran despite the bulk-sized gap",
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

    /// The connection stays up, so only a deadline can drive the retry.
    #[tokio::test(start_paused = true)]
    async fn hung_head_fetch_fails_by_deadline_and_the_task_retries() {
        use crate::test_utils::FakeNode;

        // FakeNode parks `eth_blockNumber` until a head exists.
        let node = FakeNode::new();
        let pool = node.pool(&[alloy_chains::Chain::mainnet()], POLL);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);

        // A second head fetch can only follow a deadline on the first.
        tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let head_fetches = node
                    .recorded_requests()
                    .iter()
                    .filter(|req| req.method == "eth_blockNumber")
                    .count();
                if head_fetches >= 2 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the deadlined head fetch must be retried");

        node.push_chain_log(log_at(1));
        let log = recv(&mut stream).await;
        assert_eq!(log.block_number, Some(1));
        tasks.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn parked_tail_probe_fails_by_deadline_and_the_task_retries() {
        use crate::test_utils::FakeNode;

        let node = FakeNode::new();
        node.push_chain_log(log_at(1));
        let pool = node.pool(&[alloy_chains::Chain::mainnet()], POLL);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);

        // Reach a delivered tail.
        let log = recv(&mut stream).await;
        assert_eq!(log.block_number, Some(1));
        let probe_count = |node: &FakeNode| {
            node.recorded_requests()
                .iter()
                .filter(|req| req.method == "eth_getBlockByNumber")
                .count()
        };
        let probes_before = probe_count(&node);

        // `fail_head_fetches` ends the stream, so the loop re-probes the
        // tail; no await separates the two calls, so the one-shot delay can
        // only land on that probe.
        node.delay_next_method(
            nexum_world::ChainMethod::EthGetBlockByNumber,
            Duration::from_secs(3600),
        );
        node.fail_head_fetches(1);

        // A second probe can only follow a deadline on the first.
        tokio::time::timeout(Duration::from_secs(600), async {
            while probe_count(&node) < probes_before + 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the deadlined tail probe must be retried");

        let requests = node.recorded_requests();
        let last_probe = requests
            .iter()
            .rposition(|req| req.method == "eth_getBlockByNumber")
            .expect("the retried probe is recorded");
        let parked_probe = requests[..last_probe]
            .iter()
            .rposition(|req| req.method == "eth_getBlockByNumber")
            .expect("the deadlined probe is recorded");
        assert!(
            !requests[parked_probe..last_probe]
                .iter()
                .any(|req| req.method == "eth_getLogs"),
            "an unconfirmed tail never re-opens the poller",
        );

        node.push_chain_log(log_at(2));
        let log = recv(&mut stream).await;
        assert!(!log.removed, "a confirmed tail resumes without retracting");
        assert_eq!(log.block_number, Some(2));
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

    /// `open_chain_log_streams` spawns one reconnect task per event source.
    #[tokio::test]
    async fn open_chain_log_streams_opens_one_task_per_source() {
        let rpc = MockRpc::new();
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let sources = vec![
            EventSource {
                module: ModuleId::parse("mod-a").expect("valid module name"),
                chain: alloy_chains::Chain::mainnet(),
                filter: alloy_rpc_types_eth::Filter::default(),
                cursor_key: None,
                initial_cursor: None,
                max_lookback: None,
            },
            EventSource {
                module: ModuleId::parse("mod-b").expect("valid module name"),
                chain: alloy_chains::Chain::mainnet(),
                filter: alloy_rpc_types_eth::Filter::default(),
                cursor_key: None,
                initial_cursor: None,
                max_lookback: None,
            },
        ];
        let streams = open_chain_log_streams(&pool, sources, &executor, &mut tasks);
        assert_eq!(streams.len(), 2, "one stream per source");
        tasks.shutdown().await;
    }

    /// A reconnect task whose receiver drops exits on its own with
    /// [`TaskExit::ReceiverGone`], not via abort.
    #[tokio::test(start_paused = true)]
    async fn reconnect_task_exits_receiver_gone_when_receiver_drops() {
        let rpc = MockRpc::new();
        // The failing `tx.send` against the dropped receiver is the exit
        // path under test.
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

    /// Long enough for one failed open and its backoff line, short of the
    /// half-second minimum backoff that would start a second attempt.
    const ONE_ATTEMPT: Duration = Duration::from_millis(400);

    #[tokio::test(start_paused = true)]
    async fn a_block_source_line_names_its_kind_on_the_span_alone() {
        let pool = pool_for(&MockRpc::new());
        let (tx, _rx) = mpsc::channel(1);
        let sink = JsonLogs::default();

        // The task builds its own span, so it must be called under the
        // collector rather than wrapped after the fact.
        let _ = tokio::time::timeout(
            ONE_ATTEMPT,
            async { reconnecting_block_task(pool, alloy_chains::Chain::mainnet(), tx).await }
                .with_subscriber(json_collector(sink.clone(), tracing::Level::WARN)),
        )
        .await;

        let line = sink.line("block source open failed");
        assert_eq!(line["span"]["source_kind"], SOURCE_KIND_BLOCK);
        assert_eq!(line["span"]["name"], "source");
        assert!(
            line.get("source_kind").is_none(),
            "the kind lives on the span, never per site: {line}",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_chain_log_line_names_its_kind_on_the_span_alone() {
        let pool = pool_for(&MockRpc::new());
        let (tx, _rx) = mpsc::channel(1);
        let sink = JsonLogs::default();

        let _ = tokio::time::timeout(
            ONE_ATTEMPT,
            async {
                reconnecting_chain_log_task(
                    pool,
                    ModuleId::parse("mod-a").expect("valid module name"),
                    alloy_chains::Chain::mainnet(),
                    alloy_rpc_types_eth::Filter::default(),
                    ChainLogResume {
                        cursor_key: None,
                        initial_cursor: None,
                        max_lookback: None,
                    },
                    tx,
                )
                .await
            }
            .with_subscriber(json_collector(sink.clone(), tracing::Level::WARN)),
        )
        .await;

        let line = sink.line("event source head fetch failed");
        assert_eq!(line["span"]["source_kind"], SOURCE_KIND_CHAIN_LOG);
        assert_eq!(line["span"]["name"], "source");
        assert!(
            line.get("source_kind").is_none(),
            "the kind lives on the span, never per site: {line}",
        );
        assert_eq!(line["module"], "mod-a");
    }

    #[tokio::test(start_paused = true)]
    async fn block_source_reopens_after_a_failed_open() {
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
        .expect("the reopened source delivers");
        assert_eq!(header.number, 5);
        tasks.shutdown().await;
    }

    /// A header on the block source records the head under its `chain_id` label.
    #[test]
    fn a_block_header_sets_the_chain_head_gauge() {
        use crate::test_utils::metrics_util::debugging::DebugValue;
        use crate::test_utils::{capture_metrics, samples_named};

        let (number, samples) = capture_metrics(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("current-thread runtime")
                .block_on(async {
                    let rpc = MockRpc::new();
                    rpc.push_script(vec![rpc_head(5), rpc_head(5), rpc_ok(&linked_block(5))]);
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
                    let header = loop {
                        match stream.next().await.expect("stream alive") {
                            Ok((_, header)) => break header,
                            Err(_) => continue,
                        }
                    };
                    tasks.shutdown().await;
                    header.number
                })
        });
        assert_eq!(number, 5);
        let hits = samples_named(&samples, "nexum_runtime_chain_head_height");
        assert_eq!(hits.len(), 1, "one series: {samples:?}");
        assert!(hits[0].has_label("chain_id", "1"), "{:?}", hits[0].labels);
        assert!(
            matches!(hits[0].value, DebugValue::Gauge(v) if v.0 == 5.0),
            "{:?}",
            hits[0].value,
        );
    }

    #[test]
    fn a_backfill_does_not_lower_the_chain_head_gauge() {
        use crate::test_utils::metrics_util::debugging::DebugValue;
        use crate::test_utils::{capture_metrics, samples_named};

        let ((), samples) = capture_metrics(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("current-thread runtime")
                .block_on(async {
                    let rpc = MockRpc::new();
                    rpc.push_script(attempt(
                        3,
                        None,
                        vec![cycle(3, &[(1, vec![log_at(1)]), (2, vec![log_at(2)])])],
                    ));
                    let pool = pool_for(&rpc);
                    let manager = TaskManager::new();
                    let executor = manager.executor();
                    let mut tasks = TaskSet::new();
                    let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, Some(1));
                    assert_eq!(recv(&mut stream).await.block_number, Some(1));
                    assert_eq!(recv(&mut stream).await.block_number, Some(2));
                    tasks.shutdown().await;
                })
        });
        let hits = samples_named(&samples, "nexum_runtime_chain_head_height");
        assert_eq!(hits.len(), 1, "one series: {samples:?}");
        assert!(hits[0].has_label("chain_id", "1"), "{:?}", hits[0].labels);
        assert!(
            matches!(hits[0].value, DebugValue::Gauge(v) if v.0 == 3.0),
            "batches 1 and 2 leave the open-time head 3 in place: {:?}",
            hits[0].value,
        );
    }

    /// On a chain with no block source, the outer head probe reruns only on a reopen.
    #[test]
    fn pumped_batches_advance_the_chain_head_gauge_past_the_open_time_head() {
        use crate::test_utils::metrics_util::debugging::DebugValue;
        use crate::test_utils::{capture_metrics, samples_named};

        let ((), samples) = capture_metrics(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("current-thread runtime")
                .block_on(async {
                    let rpc = MockRpc::new();
                    rpc.push_script(attempt(
                        3,
                        None,
                        vec![
                            cycle(3, &[(3, vec![log_at(3)])]),
                            cycle(4, &[(4, vec![log_at(4)])]),
                        ],
                    ));
                    let pool = pool_for(&rpc);
                    let manager = TaskManager::new();
                    let executor = manager.executor();
                    let mut tasks = TaskSet::new();
                    let mut stream = spawn_chain_log_task(&pool, &executor, &mut tasks, None);
                    assert_eq!(recv(&mut stream).await.block_number, Some(3));
                    assert_eq!(recv(&mut stream).await.block_number, Some(4));
                    tasks.shutdown().await;
                })
        });
        let hits = samples_named(&samples, "nexum_runtime_chain_head_height");
        assert_eq!(hits.len(), 1, "one series: {samples:?}");
        assert!(hits[0].has_label("chain_id", "1"), "{:?}", hits[0].labels);
        assert!(
            matches!(hits[0].value, DebugValue::Gauge(v) if v.0 == 4.0),
            "the batch at 4 advances the gauge without a reopen: {:?}",
            hits[0].value,
        );
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
            poller_start_block(None, None, None, 100, None).start_block,
            100,
            "first open starts at head, no history replay",
        );
    }

    #[test]
    fn poller_start_block_resumes_from_the_scanned_basis() {
        let start = poller_start_block(None, Some(91), None, 100, None);
        assert_eq!(
            start.start_block, 91,
            "a re-open with a confirmed tail resumes from the scanned basis",
        );
        assert!(!start.restates_tail, "a confirmed tail is never restated");
    }

    #[test]
    fn poller_start_block_restarts_at_an_invalidated_tail() {
        let start = poller_start_block(None, Some(96), Some(88), 100, None);
        assert_eq!(
            start.start_block, 88,
            "a tail within the revalidation depth replays AT its height, below the scanned basis",
        );
        assert!(
            start.restates_tail,
            "the restart at the tail retracts its logs so the scan restates them",
        );
    }

    #[test]
    fn poller_start_block_retracts_a_tail_exactly_at_the_bound() {
        let basis = 5_000;
        let tail = basis - REVALIDATE_DEPTH;
        let start = poller_start_block(None, Some(basis), Some(tail), 5_000, None);
        assert_eq!(
            start.start_block, tail,
            "the bound is inclusive - a tail exactly REVALIDATE_DEPTH below the basis replays",
        );
        assert!(start.restates_tail);
    }

    #[test]
    fn poller_start_block_bounds_a_tail_deeper_than_the_revalidation_depth() {
        let start = poller_start_block(None, Some(5_000), Some(100), 5_000, None);
        assert_eq!(
            start.start_block,
            5_000 - REVALIDATE_DEPTH,
            "a tail deeper than the revalidation depth restarts at the bound, not at the tail",
        );
        assert!(
            !start.restates_tail,
            "a tail below the bound cannot be restated; the caller reports it terminal",
        );
    }

    #[test]
    fn poller_start_block_retracts_a_tail_within_the_bound_of_the_lookback_floor() {
        // The floor at 1_030 sits above the basis 1_000, so the bound hangs
        // off the floor.
        let start = poller_start_block(None, Some(1_000), Some(990), 2_000, Some(1_030));
        assert_eq!(
            start.start_block, 990,
            "a tail within the revalidation depth of the lookback floor replays AT its height",
        );
        assert!(start.restates_tail);
    }

    #[test]
    fn poller_start_block_bounds_a_tail_against_the_lookback_floor() {
        let floor = 99_990;
        let start = poller_start_block(None, Some(1_000), Some(990), 100_000, Some(floor));
        assert_eq!(
            start.start_block,
            floor - REVALIDATE_DEPTH,
            "a tail more than the revalidation depth below the floor restarts at the bound",
        );
        assert!(
            !start.restates_tail,
            "the clamp exemption may not exceed the cap by more than the revalidation depth",
        );
    }

    #[test]
    fn poller_start_block_ignores_a_lookback_floor_below_the_basis() {
        let start = poller_start_block(None, Some(96), Some(88), 100, Some(90));
        assert_eq!(
            start.start_block, 88,
            "a floor below the basis leaves the basis-relative bound in charge",
        );
        assert!(start.restates_tail);
    }

    #[test]
    fn poller_start_block_does_not_clamp_the_reconnect_arm_to_head() {
        // Alloy parks safely when the start is past head.
        assert_eq!(
            poller_start_block(None, Some(151), None, 100, None).start_block,
            151,
            "no head clamp on the reconnect arm",
        );
    }

    #[test]
    fn poller_start_block_boot_cursor_resumes_at_the_cursor() {
        assert_eq!(
            poller_start_block(Some(42), None, None, 100, None).start_block,
            42,
            "the persisted cursor replays AT its block, not after it",
        );
    }

    #[test]
    fn poller_start_block_boot_cursor_clamps_to_head() {
        assert_eq!(
            poller_start_block(Some(150), None, None, 100, None).start_block,
            100,
            "a cursor a reorg left past head starts at head and catches up",
        );
    }

    #[test]
    fn poller_start_block_treats_a_genesis_basis_as_history() {
        assert_eq!(
            poller_start_block(None, Some(0), None, 5_000, None).start_block,
            0,
            "an open at block 0 re-opens at block 0, not at head",
        );
    }

    /// An engine declaring only one stream kind must not bail at boot when
    /// the other stream set is empty.
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

        // A regression bails immediately on the empty stream's first `None`;
        // correct behaviour blocks on `shutdown` for the full 50 ms.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "run returned in {elapsed:?}, expected >= ~50ms (shutdown timer)",
        );
    }

    /// The `biased` select must drain both stream kinds in one `run()`
    /// session without starving either.
    #[tokio::test]
    async fn run_delivers_block_and_chain_log_events_without_starvation() {
        use std::time::Duration;

        use alloy_chains::Chain;
        use alloy_rpc_types_eth::Filter;

        use crate::runtime::event_loop::{open_block_streams, open_chain_log_streams, run};
        use crate::test_utils::FakeNode;
        use nexum_runtime_chain::ProviderPool;
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

        // Pre-push one event of each kind so `run()` drains both on its first pass.
        block_node.push_block(alloy_rpc_types_eth::Header::default());
        log_node.push_chain_log(alloy_rpc_types_eth::Log::default());

        let block_streams = open_block_streams(&pool, &[Chain::mainnet()], &executor, &mut tasks);
        let event_sources = vec![crate::supervisor::EventSource {
            module: ModuleId::parse("test-module").expect("valid module name"),
            chain: Chain::from_id(100),
            filter: Filter::default(),
            cursor_key: None,
            initial_cursor: None,
            max_lookback: None,
        }];
        let chain_log_streams = open_chain_log_streams(&pool, event_sources, &executor, &mut tasks);

        // 500 ms only bounds wall time; the assertion is on the tally, so a
        // miss means a broken select arm, not a slow scheduler.
        let shutdown = tokio::time::sleep(Duration::from_millis(500));
        let outcome = tokio::time::timeout(
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
        assert_eq!(
            outcome.dispatched_blocks, 1,
            "the queued block must be drained and dispatched",
        );
        assert_eq!(
            outcome.dispatched_events, 1,
            "the queued chain-log must be drained and dispatched",
        );
        assert!(matches!(outcome.end, RunEnd::Shutdown), "{:?}", outcome.end);
    }

    /// On the shutdown path `run()` aborts and joins every reconnect task, so
    /// none detaches and outlives the engine.
    #[tokio::test]
    async fn run_drains_reconnect_tasks_cleanly_on_shutdown() {
        use std::time::Duration;

        use alloy_chains::Chain;

        use crate::runtime::event_loop::{open_block_streams, run};
        use crate::test_utils::FakeNode;
        use nexum_tasks::{TaskManager, TaskSet};

        let mut booted = boot_mock_supervisor().await;
        let pool = FakeNode::new().pool(
            &[Chain::mainnet(), Chain::from_id(100)],
            Duration::from_millis(20),
        );
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();

        // Two stream tasks: both must drain before `run()` returns.
        let block_streams = open_block_streams(
            &pool,
            &[Chain::mainnet(), Chain::from_id(100)],
            &executor,
            &mut tasks,
        );

        let shutdown = tokio::time::sleep(Duration::from_millis(10));
        // Without the drain the reconnect tasks detach; if the drain hangs,
        // the timeout fails fast instead of stalling the suite.
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

    /// The module-owned path through a real source rather than a stub exit.
    #[tokio::test(start_paused = true)]
    async fn a_module_owned_terminal_exit_poisons_its_module_and_ends_an_event_only_run() {
        use crate::test_utils::{BootScenario, LocalTypes, TestManifest, example_wasm_or_skip};

        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let mut booted = BootScenario::<LocalTypes>::new()
            .wasm(&wasm)
            .module(TestManifest::new("example").cap("logging").block_trigger(1))
            .boot()
            .await
            .expect("boot the example module");

        let rpc = MockRpc::new();
        let empty: Vec<(u64, Vec<Log>)> = (91..=155).map(|n| (n, Vec::new())).collect();
        rpc.push_script(attempt(
            90,
            None,
            vec![
                cycle(90, &[(90, vec![log_at(90)])]),
                cycle(155, &empty),
                vec![rpc_err("stream torn down")],
            ],
        ));
        let pool = pool_for(&rpc);
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        let sources = vec![EventSource {
            module: ModuleId::parse("example").expect("valid module name"),
            chain: alloy_chains::Chain::mainnet(),
            filter: alloy_rpc_types_eth::Filter::default(),
            cursor_key: None,
            initial_cursor: None,
            max_lookback: None,
        }];
        let chain_log_streams = open_chain_log_streams(&pool, sources, &executor, &mut tasks);

        // Push the reorged reconnect script only once phase 1 has drained, so
        // the torn-down stream cannot consume it.
        let phase_two = rpc.clone();
        let fork_hash = B256::repeat_byte(0xcc);
        let mut fork_block = linked_block(90);
        fork_block.header.hash = fork_hash;
        let _pusher = executor.spawn(async move {
            while phase_two.pending() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            phase_two.push_script(attempt(156, Some(rpc_ok(&fork_block)), Vec::new()));
            TaskExit::ReceiverGone
        });

        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            run(
                &mut booted.supervisor,
                Vec::new(),
                chain_log_streams,
                Vec::new(),
                tasks,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("run() must return without an external shutdown");

        assert!(
            matches!(outcome.end, RunEnd::NothingLive),
            "the only source is terminal and nothing else feeds the loop: {:?}",
            outcome.end,
        );
        assert_eq!(
            outcome.dispatched_events, 1,
            "the ordinary delivery at 90 reached the module before the terminal report",
        );
        assert_eq!(
            booted.supervisor.poisoned_count(),
            1,
            "the terminal report quarantined the source's module",
        );
        assert_eq!(booted.supervisor.alive_count(), 0);
    }

    /// A shared exit is one with no owning module.
    #[tokio::test]
    async fn a_shared_source_terminal_exit_ends_run_with_the_reason() {
        let mut booted = boot_mock_supervisor().await;
        let manager = TaskManager::new();
        let executor = manager.executor();
        let mut tasks = TaskSet::new();
        tasks.push(
            "block:1",
            executor.spawn(async {
                TaskExit::SourceTerminal(SourceTermination {
                    module: None,
                    chain_id: 1,
                    reason: "endpoint no longer serves chain 1".to_owned(),
                })
            }),
        );

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run(
                &mut booted.supervisor,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                tasks,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("a shared terminal exit must end run without a shutdown");

        let RunEnd::SourceTerminal(term) = outcome.end else {
            panic!("expected a terminal end, got {:?}", outcome.end);
        };
        assert_eq!(term.module, None);
        assert_eq!(term.chain_id, 1);
        assert_eq!(term.reason, "endpoint no longer serves chain 1");
    }

    /// A set of one task, reporting `module` and `reason` after a delay.
    fn delayed_terminal_task(module: Option<&str>, reason: &str) -> (TaskManager, TaskSet) {
        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let module = module.map(str::to_owned);
        let reason = reason.to_owned();
        tasks.push(
            "chain-log:1:mod",
            manager.executor().spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                TaskExit::SourceTerminal(SourceTermination {
                    module,
                    chain_id: 1,
                    reason,
                })
            }),
        );
        (manager, tasks)
    }

    #[tokio::test(start_paused = true)]
    async fn a_module_owned_terminal_exit_keeps_the_loop_live_while_blocks_remain() {
        let mut booted = boot_mock_supervisor().await;
        let (manager, mut tasks) = delayed_terminal_task(Some("mod"), "unrecoverable");
        let (tx, rx) = mpsc::channel::<TaggedChainLog>(1);
        // The channel closes when the terminal task's report resolves.
        let holder = manager.executor().spawn(async move {
            let _tx = tx;
            tokio::time::sleep(Duration::from_millis(10)).await;
            TaskExit::ReceiverGone
        });
        tasks.push("block:1", holder);
        let chain_log_streams: Vec<TaggedChainLogStream> = vec![Box::pin(receiver_stream(rx))];
        let block_streams: Vec<TaggedBlockStream> = vec![Box::pin(futures::stream::pending())];

        let shutdown = tokio::time::sleep(Duration::from_millis(200));
        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
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
        assert!(
            matches!(outcome.end, RunEnd::Shutdown),
            "a live block stream keeps the loop running to shutdown: {:?}",
            outcome.end,
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_chain_log_end_ahead_of_its_terminal_report_is_not_misread() {
        let mut booted = boot_mock_supervisor().await;
        let (_manager, tasks) = delayed_terminal_task(Some("mod"), "unrecoverable");
        // The stream is already closed when `run` first polls it; the
        // report resolves only later.
        let (tx, rx) = mpsc::channel::<TaggedChainLog>(1);
        drop(tx);
        let chain_log_streams: Vec<TaggedChainLogStream> = vec![Box::pin(receiver_stream(rx))];

        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            run(
                &mut booted.supervisor,
                Vec::new(),
                chain_log_streams,
                Vec::new(),
                tasks,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("run() classifies the end and returns");
        assert!(
            matches!(outcome.end, RunEnd::NothingLive),
            "the late report is absorbed, not misread as a panic: {:?}",
            outcome.end,
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_aborted_reconnect_task_ends_the_run_as_unaccounted() {
        let mut booted = boot_mock_supervisor().await;
        let manager = TaskManager::new();
        let mut tasks = TaskSet::new();
        let (tx, rx) = mpsc::channel::<TaggedChainLog>(1);
        // An aborted task yields no exit, so the set never accounts for the
        // end of the stream its dropped sender closes.
        let handle = manager.executor().spawn(async move {
            let _tx = tx;
            std::future::pending::<()>().await;
            TaskExit::ReceiverGone
        });
        handle.abort();
        tasks.push("chain-log:1:mod", handle);
        let chain_log_streams: Vec<TaggedChainLogStream> = vec![Box::pin(receiver_stream(rx))];

        let sink = LogSink::default();
        let collector = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish();
        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            run(
                &mut booted.supervisor,
                Vec::new(),
                chain_log_streams,
                Vec::new(),
                tasks,
                std::future::pending::<()>(),
            )
            .with_subscriber(collector),
        )
        .await
        .expect("run() classifies the end and returns");
        assert!(
            matches!(outcome.end, RunEnd::StreamEnded),
            "a dead reconnect task is an unexpected end, not a clean stop: {:?}",
            outcome.end,
        );
        let logged = sink.text();
        let line = logged
            .lines()
            .find(|line| line.contains("reconnect task ended unexpectedly"))
            .expect("the unaccounted end warns");
        assert!(
            line.contains("chain-log:1:mod"),
            "the warning names the dead pump: {line}",
        );
    }

    #[test]
    fn dead_task_names_reads_unknown_when_nothing_died() {
        assert_eq!(dead_task_names(&[]), "unknown");
        assert_eq!(
            dead_task_names(&["block:1".into(), "chain-log:1:mod".into()]),
            "block:1, chain-log:1:mod",
        );
    }

    /// Collects the console output of a `with_subscriber` future.
    #[derive(Clone, Default)]
    struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl LogSink {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("sink is not poisoned").clone();
            String::from_utf8(bytes).expect("log output is UTF-8")
        }
    }

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("sink is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = LogSink;

        fn make_writer(&'a self) -> LogSink {
            self.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_block_end_ahead_of_its_terminal_report_still_carries_the_reason() {
        let mut booted = boot_mock_supervisor().await;
        let (_manager, tasks) = delayed_terminal_task(None, "endpoint no longer serves chain 1");
        let block_streams: Vec<TaggedBlockStream> = vec![Box::pin(futures::stream::empty())];

        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            run(
                &mut booted.supervisor,
                block_streams,
                Vec::new(),
                Vec::new(),
                tasks,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("run() classifies the end and returns");
        let RunEnd::SourceTerminal(term) = outcome.end else {
            panic!("expected a terminal end, got {:?}", outcome.end);
        };
        assert_eq!(term.module, None);
        assert_eq!(term.reason, "endpoint no longer serves chain 1");
    }
}
