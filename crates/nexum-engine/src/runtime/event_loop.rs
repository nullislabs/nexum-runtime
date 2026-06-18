//! Open live `eth_subscribe` streams and dispatch their events to the
//! supervisor until a shutdown signal arrives.

use futures::StreamExt;
use futures::stream::{BoxStream, FuturesUnordered, select_all};
use tracing::{info, warn};

use crate::bindings::nexum;
use crate::host::provider_pool::ProviderPool;
use crate::supervisor::Supervisor;

/// Per-chain block subscriptions, one shared stream per chain id.
pub async fn open_block_streams(
    pool: &ProviderPool,
    chains: &std::collections::BTreeSet<u64>,
) -> Vec<TaggedBlockStream> {
    let mut openings: FuturesUnordered<_> = chains
        .iter()
        .copied()
        .map(|chain_id| async move { (chain_id, pool.subscribe_blocks(chain_id).await) })
        .collect();

    let mut streams = Vec::new();
    while let Some((chain_id, result)) = openings.next().await {
        match result {
            Ok(stream) => {
                info!(chain_id, "block subscription open");
                let tagged: TaggedBlockStream = Box::pin(stream.map(move |item| {
                    item.map(|header| (chain_id, header))
                        .map_err(anyhow::Error::from)
                }));
                streams.push(tagged);
            }
            Err(err) => {
                warn!(chain_id, error = %err, "block subscription failed");
            }
        }
    }
    streams
}

/// Per-module log subscriptions. Each entry is a stream tagged with
/// the owning module name + chain id.
pub async fn open_log_streams(
    pool: &ProviderPool,
    subs: Vec<(String, u64, alloy_rpc_types_eth::Filter)>,
) -> Vec<TaggedLogStream> {
    let mut openings: FuturesUnordered<_> = subs
        .into_iter()
        .map(|(module, chain_id, filter)| async move {
            let stream = pool.subscribe_logs(chain_id, filter).await;
            (module, chain_id, stream)
        })
        .collect();

    let mut streams = Vec::new();
    while let Some((module, chain_id, result)) = openings.next().await {
        match result {
            Ok(stream) => {
                info!(module = %module, chain_id, "log subscription open");
                let module_name = module.clone();
                let tagged: TaggedLogStream = Box::pin(stream.map(move |item| {
                    item.map(|log| (module_name.clone(), chain_id, log))
                        .map_err(anyhow::Error::from)
                }));
                streams.push(tagged);
            }
            Err(err) => {
                warn!(module = %module, chain_id, error = %err, "log subscription failed");
            }
        }
    }
    streams
}

pub type TaggedBlockStream = std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<(u64, alloy_rpc_types_eth::Header), anyhow::Error>>
            + Send,
    >,
>;
pub type TaggedLogStream = std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<(String, u64, alloy_rpc_types_eth::Log), anyhow::Error>>
            + Send,
    >,
>;

/// Drive the supervisor with events until `shutdown` resolves.
pub async fn run(
    supervisor: &mut Supervisor,
    block_streams: Vec<TaggedBlockStream>,
    log_streams: Vec<TaggedLogStream>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    // `select_all` over an empty Vec yields `None` immediately, which
    // would trip the "stream ended -> shut down" arm below before the
    // first block / log ever flows. Engine configs that subscribe to
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
    let mut logs: BoxStream<'_, _> = if log_streams.is_empty() {
        futures::stream::pending().boxed()
    } else {
        select_all(log_streams).boxed()
    };
    let mut shutdown = Box::pin(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => return,
            next = blocks.next() => match next {
                Some(Ok((chain_id, header))) => {
                    let block = nexum::host::types::Block {
                        chain_id,
                        number: header.number,
                        hash: header.hash.as_slice().to_vec(),
                        timestamp: header.timestamp.saturating_mul(1000),
                    };
                    supervisor.dispatch_block(block).await;
                }
                Some(Err(err)) => warn!(error = %err, "block stream error - continuing"),
                None => {
                    // alloy ends the stream with None when the
                    // WebSocket drops. Without this branch the loop
                    // keeps polling a dead stream and the operator
                    // sees no events with no indication anything is
                    // wrong. Bail out so the supervisor (or whatever
                    // wraps the engine) restarts us; a reconnect-
                    // with-backoff is the 0.3 fix.
                    warn!("block stream ended (WebSocket dropped?) - shutting down for restart");
                    return;
                }
            },
            next = logs.next() => match next {
                Some(Ok((module, chain_id, log))) => {
                    supervisor.dispatch_log(&module, chain_id, log).await;
                }
                Some(Err(err)) => warn!(error = %err, "log stream error - continuing"),
                None => {
                    warn!("log stream ended (WebSocket dropped?) - shutting down for restart");
                    return;
                }
            },
        }
    }
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
