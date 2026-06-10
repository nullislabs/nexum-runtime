#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod engine_config;
mod host;
mod manifest;
mod supervisor;

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use futures::StreamExt;
use futures::stream::{FuturesUnordered, select_all};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wasmtime::Engine;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::error::Context as _;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

/// Reference CLI for the 0.2 `nexum-engine` runtime.
///
/// Loads a Wasm Component, links the `shepherd:cow/shepherd` host
/// world plus the WASI p2 set, calls `init` once, then dispatches a
/// single synthetic block event so the host stubs exercise their
/// timing paths. Production deployments invoke the engine through
/// the supervisor entrypoint introduced in later milestones; this
/// CLI is the M1 smoke-test surface.
#[derive(Parser, Debug)]
#[command(
    name = "nexum-engine",
    about = "Load a Wasm Component and dispatch a synthetic block event",
    long_about = None,
    version,
)]
struct Cli {
    /// Optional path to a Wasm Component file. Backwards-compat
    /// shortcut that synthesises a one-module engine config when no
    /// `--engine-config` is supplied. Production deployments declare
    /// modules in TOML and omit this positional.
    wasm: Option<PathBuf>,

    /// Optional explicit path to the module's `nexum.toml` manifest.
    /// Only consulted alongside the positional `wasm` shortcut; when
    /// the engine config drives the module set, each module brings
    /// its own manifest via the engine-config entries.
    manifest: Option<PathBuf>,

    /// Optional explicit path to the engine-wide `engine.toml` config.
    /// When omitted, the engine resolves the default search path
    /// documented in `engine_config::load_or_default`.
    #[arg(long = "engine-config")]
    engine_config: Option<PathBuf>,
}

// Both packages are listed explicitly so wit-parser can resolve the
// cross-package reference natively - no vendored deps/ tree needed.
// World name is fully qualified.
wasmtime::component::bindgen!({
    path: ["../../wit/nexum-host", "../../wit/shepherd-cow"],
    world: "shepherd:cow/shepherd",
    imports: { default: async },
    exports: { default: async },
});

use nexum::host::types::HostErrorKind;

/// Default fuel budget granted per `on_event` invocation (≈ 1 billion WASM
/// instructions). Modules that exceed this budget trap with `OutOfFuel`.
/// Configurable per-module via `engine.toml` in 0.3.
pub const DEFAULT_FUEL_PER_EVENT: u64 = 1_000_000_000;

/// Default linear-memory cap per module store (64 MiB). Prevents a single
/// runaway module from exhausting process memory. Configurable in 0.3.
pub const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    /// Wasmtime memory/table/instance resource limits for this store.
    limits: wasmtime::StoreLimits,
    /// Origin for `clock::monotonic-ns`. Differences between successive
    /// readings are the only meaningful values.
    monotonic_baseline: Instant,
    /// Per-module `[capabilities.http].allow` allowlist (from module.toml).
    /// Consulted by `http::fetch` before any outbound call.
    http_allowlist: Vec<String>,
    /// Namespace for the running module's `local-store` rows. Set from
    /// `manifest.module.name` at instantiation.
    module_namespace: String,
    /// `cow-api` backend - per-chain `OrderBookApi` clients + reqwest.
    cow: host::cow_orderbook::OrderBookPool,
    /// `chain` backend - per-chain alloy `DynProvider` pool.
    chain: host::provider_pool::ProviderPool,
    /// `local-store` backend - redb file with host-side namespacing.
    store: host::local_store_redb::LocalStore,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn unimplemented(domain: &str, detail: impl Into<String>) -> HostError {
    HostError {
        domain: domain.into(),
        kind: HostErrorKind::Unsupported,
        code: 501,
        message: detail.into(),
        data: None,
    }
}

fn internal_error(domain: &str, detail: impl Into<String>) -> HostError {
    HostError {
        domain: domain.into(),
        kind: HostErrorKind::Internal,
        code: 0,
        message: detail.into(),
        data: None,
    }
}

// -- nexum:host/types is empty (declarations only). --

impl nexum::host::types::Host for HostState {}

// -- shepherd:cow/cow-api: REST passthrough + typed submission. --

impl shepherd::cow::cow_api::Host for HostState {
    async fn request(
        &mut self,
        chain_id: u64,
        method: String,
        path: String,
        body: Option<String>,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, %method, %path, "cow-api::request");
        let result = match self
            .cow
            .request(chain_id, &method, &path, body.as_deref())
            .await
        {
            Ok(body) => Ok(body),
            Err(host::cow_orderbook::CowApiError::UnknownChain(id)) => Err(unimplemented(
                "cow-api",
                format!("chain {id} not in cowprotocol"),
            )),
            Err(host::cow_orderbook::CowApiError::BadMethod(m)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: format!("unsupported HTTP method: {m}"),
                data: None,
            }),
            Err(host::cow_orderbook::CowApiError::BadPath(msg)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: msg,
                data: None,
            }),
            Err(err) => Err(internal_error("cow-api", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "cow-api::request done");
        result
    }

    async fn submit_order(
        &mut self,
        chain_id: u64,
        order_data: Vec<u8>,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, bytes = order_data.len(), "cow-api::submit-order");
        let result = match self.cow.submit_order_json(chain_id, &order_data).await {
            Ok(uid) => Ok(format!("0x{}", hex_encode(uid.as_slice()))),
            Err(host::cow_orderbook::CowApiError::UnknownChain(id)) => Err(unimplemented(
                "cow-api",
                format!("chain {id} not in cowprotocol"),
            )),
            Err(host::cow_orderbook::CowApiError::Decode(err)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: format!("invalid OrderCreation JSON: {err}"),
                data: None,
            }),
            Err(host::cow_orderbook::CowApiError::Orderbook(err)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::Denied,
                code: 0,
                message: err.to_string(),
                data: None,
            }),
            Err(err) => Err(internal_error("cow-api", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "cow-api::submit-order done");
        result
    }
}

// -- nexum:host/chain: raw JSON-RPC dispatch over alloy. --

impl nexum::host::chain::Host for HostState {
    async fn request(
        &mut self,
        chain_id: u64,
        method: String,
        params: String,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, %method, "chain::request");
        let result = match self.chain.request(chain_id, method.clone(), params).await {
            Ok(body) => Ok(body),
            Err(host::provider_pool::ProviderError::UnknownChain(id)) => Err(HostError {
                domain: "chain".into(),
                kind: HostErrorKind::Unsupported,
                code: 0,
                message: format!("chain {id} has no engine.toml RPC entry"),
                data: None,
            }),
            Err(host::provider_pool::ProviderError::InvalidParams { detail, .. }) => {
                Err(HostError {
                    domain: "chain".into(),
                    kind: HostErrorKind::InvalidInput,
                    code: -32602,
                    message: detail,
                    data: None,
                })
            }
            Err(host::provider_pool::ProviderError::Rpc { detail, .. }) => Err(HostError {
                domain: "chain".into(),
                kind: HostErrorKind::Internal,
                code: -32603,
                message: detail,
                data: None,
            }),
            Err(err) => Err(internal_error("chain", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request done");
        result
    }

    async fn request_batch(
        &mut self,
        chain_id: u64,
        requests: Vec<nexum::host::chain::RpcRequest>,
    ) -> Result<Vec<nexum::host::chain::RpcResult>, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, count = requests.len(), "chain::request-batch");
        let mut out = Vec::with_capacity(requests.len());
        for req in requests {
            match nexum::host::chain::Host::request(self, chain_id, req.method, req.params).await {
                Ok(s) => out.push(nexum::host::chain::RpcResult::Ok(s)),
                Err(e) => out.push(nexum::host::chain::RpcResult::Err(e)),
            }
        }
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request-batch done");
        Ok(out)
    }
}

// -- nexum:host/identity: deferred to 0.3 (keystore/KMS backend). --

impl nexum::host::identity::Host for HostState {
    async fn accounts(&mut self) -> Result<Vec<Vec<u8>>, HostError> {
        // No keystore wired yet - return an empty roster so guests can
        // probe-then-skip without erroring. Real keystore lands in 0.3.
        Ok(vec![])
    }

    async fn sign(&mut self, _account: Vec<u8>, _message: Vec<u8>) -> Result<Vec<u8>, HostError> {
        Err(unimplemented("identity", "sign requires a keystore (0.3)"))
    }

    async fn sign_typed_data(
        &mut self,
        _account: Vec<u8>,
        _typed_data: String,
    ) -> Result<Vec<u8>, HostError> {
        Err(unimplemented(
            "identity",
            "sign-typed-data requires a keystore (0.3)",
        ))
    }
}

// -- nexum:host/local-store: redb backend with host-side namespacing. --

impl nexum::host::local_store::Host for HostState {
    async fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, HostError> {
        self.store
            .get(&self.module_namespace, &key)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), HostError> {
        self.store
            .set(&self.module_namespace, &key, &value)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn delete(&mut self, key: String) -> Result<(), HostError> {
        self.store
            .delete(&self.module_namespace, &key)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn list_keys(&mut self, prefix: String) -> Result<Vec<String>, HostError> {
        self.store
            .list_keys(&self.module_namespace, &prefix)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }
}

impl nexum::host::remote_store::Host for HostState {
    async fn upload(&mut self, _data: Vec<u8>) -> Result<Vec<u8>, HostError> {
        Err(unimplemented(
            "remote-store",
            "Swarm backend deferred to 0.3",
        ))
    }

    async fn download(&mut self, _reference: Vec<u8>) -> Result<Vec<u8>, HostError> {
        Err(unimplemented(
            "remote-store",
            "Swarm backend deferred to 0.3",
        ))
    }

    async fn read_feed(
        &mut self,
        _owner: Vec<u8>,
        _topic: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, HostError> {
        Err(unimplemented(
            "remote-store",
            "Swarm backend deferred to 0.3",
        ))
    }

    async fn write_feed(&mut self, _topic: Vec<u8>, _data: Vec<u8>) -> Result<Vec<u8>, HostError> {
        Err(unimplemented(
            "remote-store",
            "Swarm backend deferred to 0.3",
        ))
    }
}

impl nexum::host::messaging::Host for HostState {
    async fn publish(
        &mut self,
        _content_topic: String,
        _payload: Vec<u8>,
    ) -> Result<(), HostError> {
        Err(unimplemented("messaging", "Waku backend deferred to 0.3"))
    }

    async fn query(
        &mut self,
        _content_topic: String,
        _start_time: Option<u64>,
        _end_time: Option<u64>,
        _limit: Option<u32>,
    ) -> Result<Vec<nexum::host::types::Message>, HostError> {
        // Empty result - same posture as `identity::accounts`.
        Ok(vec![])
    }
}

impl nexum::host::logging::Host for HostState {
    async fn log(&mut self, level: nexum::host::logging::Level, message: String) {
        let module = self.module_namespace.as_str();
        match level {
            nexum::host::logging::Level::Trace => tracing::trace!(module, "{}", message),
            nexum::host::logging::Level::Debug => tracing::debug!(module, "{}", message),
            nexum::host::logging::Level::Info => tracing::info!(module, "{}", message),
            nexum::host::logging::Level::Warn => tracing::warn!(module, "{}", message),
            nexum::host::logging::Level::Error => tracing::error!(module, "{}", message),
        }
    }
}

// -- Additive 0.2 capabilities --

impl nexum::host::clock::Host for HostState {
    async fn now_ms(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    async fn monotonic_ns(&mut self) -> u64 {
        self.monotonic_baseline.elapsed().as_nanos() as u64
    }
}

impl nexum::host::random::Host for HostState {
    async fn fill(&mut self, len: u32) -> Vec<u8> {
        let mut buf = vec![0u8; len as usize];
        // getrandom 0.4: fill() returns Result<(), Error>. CSPRNG failures
        // are exceptionally rare on supported platforms; on failure we
        // return zero-filled bytes - guests that need a strong-failure
        // signal should use identity or chain primitives instead.
        let _ = getrandom::fill(&mut buf);
        buf
    }
}

impl nexum::host::http::Host for HostState {
    async fn fetch(
        &mut self,
        req: nexum::host::http::Request,
    ) -> Result<nexum::host::http::Response, HostError> {
        // Manifest allowlist enforcement runs before any I/O. Hosts that
        // never link a manifest leave `http_allowlist` empty, which denies
        // every request - matching the "no implicit network" stance.
        let host = match manifest::extract_host(&req.url) {
            Some(h) => h,
            None => {
                return Err(HostError {
                    domain: "http".into(),
                    kind: HostErrorKind::InvalidInput,
                    code: 0,
                    message: format!("not an http(s) URL: {}", req.url),
                    data: None,
                });
            }
        };
        if !manifest::host_allowed(host, &self.http_allowlist) {
            warn!(host, "[http] denied by allowlist");
            return Err(HostError {
                domain: "http".into(),
                kind: HostErrorKind::Denied,
                code: 0,
                message: format!(
                    "host {host} not in [capabilities.http].allow; \
                     add it to module.toml to permit"
                ),
                data: None,
            });
        }
        // 0.2: allowlist passed, but the reference runtime does not perform
        // real HTTP yet. Real fetch lands in 0.3.
        Err(unimplemented(
            "http",
            "fetch not implemented in 0.2 reference runtime (allowlist passed)",
        ))
    }
}

/// Lowercase hex encoder. Kept in the engine binary rather than
/// pulling a `hex` crate just for one call site.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // CLI surface (clap-derived, see `Cli`):
    //   nexum-engine [<wasm-path> [<manifest-path>]] [--engine-config <path>]
    //
    // Positional `<wasm-path>` is a backwards-compat shortcut that
    // synthesises a one-module engine config. Production deployments
    // pass `--engine-config` and declare modules in TOML.
    let cli = Cli::parse();

    let engine_cfg = engine_config::load_or_default(cli.engine_config.as_deref())?;

    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&engine_cfg.engine.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();

    info!("nexum-engine starting");

    // Bring up shared host backends.
    std::fs::create_dir_all(&engine_cfg.engine.state_dir).with_context(|| {
        format!(
            "create state directory {}",
            engine_cfg.engine.state_dir.display()
        )
    })?;
    let store_path = engine_cfg.engine.state_dir.join("local-store.redb");
    let local_store = host::local_store_redb::LocalStore::open(&store_path)
        .with_context(|| format!("open local-store at {}", store_path.display()))?;
    let cow_pool = host::cow_orderbook::OrderBookPool::default();
    let provider_pool = host::provider_pool::ProviderPool::from_config(&engine_cfg)
        .await
        .context("open chain providers")?;

    // wasmtime engine + linker - one of each, shared across modules.
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;

    let mut linker = Linker::<HostState>::new(&engine);
    Shepherd::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state| state,
    )?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Boot supervisor - `engine.toml.[[modules]]` first, CLI
    // positional second.
    let mut supervisor = if let Some(wasm) = cli.wasm.as_deref() {
        if !engine_cfg.modules.is_empty() {
            warn!("ignoring engine.toml [[modules]] because a positional <wasm-path> was given");
        }
        supervisor::Supervisor::boot_single(
            &engine,
            &linker,
            wasm,
            cli.manifest.as_deref(),
            &cow_pool,
            &provider_pool,
            &local_store,
        )
        .await?
    } else if !engine_cfg.modules.is_empty() {
        supervisor::Supervisor::boot(
            &engine,
            &linker,
            &engine_cfg,
            &cow_pool,
            &provider_pool,
            &local_store,
        )
        .await?
    } else {
        anyhow::bail!(
            "no modules to run - either pass a positional <wasm-path> or declare \
             [[modules]] entries in engine.toml"
        );
    };

    info!(
        modules = supervisor.module_count(),
        chains = supervisor.block_chains().len(),
        "supervisor ready"
    );

    // Open per-chain block subscriptions + per-module log
    // subscriptions, merge, dispatch until shutdown.
    let block_chains = supervisor.block_chains();
    let log_subs = supervisor.log_subscriptions();

    if block_chains.is_empty() && log_subs.is_empty() {
        info!("no [[subscription]] entries - engine has nothing to run; exiting");
        return Ok(());
    }

    let block_streams = open_block_streams(&provider_pool, &block_chains).await;
    let log_streams = open_log_streams(&provider_pool, log_subs).await;

    let shutdown = async {
        match wait_for_shutdown_signal().await {
            Ok(name) => info!(signal = %name, "shutdown signal received"),
            Err(err) => warn!(error = %err, "signal handler failed - using ctrl-c"),
        }
    };

    run_event_loop(&mut supervisor, block_streams, log_streams, shutdown).await;
    info!("done");
    Ok(())
}

/// Per-chain block subscriptions, one shared stream per chain id.
async fn open_block_streams(
    pool: &host::provider_pool::ProviderPool,
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
async fn open_log_streams(
    pool: &host::provider_pool::ProviderPool,
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

type TaggedBlockStream = std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<(u64, alloy_rpc_types_eth::Header), anyhow::Error>>
            + Send,
    >,
>;
type TaggedLogStream = std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<(String, u64, alloy_rpc_types_eth::Log), anyhow::Error>>
            + Send,
    >,
>;

/// Drive the supervisor with events until `shutdown` resolves.
async fn run_event_loop(
    supervisor: &mut supervisor::Supervisor,
    block_streams: Vec<TaggedBlockStream>,
    log_streams: Vec<TaggedLogStream>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    let mut blocks = select_all(block_streams);
    let mut logs = select_all(log_streams);
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
                None => {}
            },
            next = logs.next() => match next {
                Some(Ok((module, chain_id, log))) => {
                    supervisor.dispatch_log(&module, chain_id, log).await;
                }
                Some(Err(err)) => warn!(error = %err, "log stream error - continuing"),
                None => {}
            },
        }
    }
}

/// Wait for SIGINT or (on Unix) SIGTERM, whichever arrives first.
async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
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
