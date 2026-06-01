mod engine_config;
mod host;
mod manifest;

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::error::Context as _;
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
    /// Path to the Wasm Component file to load.
    wasm_path: PathBuf,

    /// Optional explicit path to the module's `nexum.toml` manifest.
    /// When omitted, the engine looks for `nexum.toml` next to the
    /// component file and falls back to a permissive default (with
    /// a deprecation warning) when none is found.
    manifest_path: Option<PathBuf>,

    /// Optional explicit path to the engine-wide `engine.toml` config.
    /// When omitted, the engine resolves the default search path
    /// documented in `engine_config::load_or_default`.
    engine_config_path: Option<PathBuf>,
}

// Both packages are listed explicitly so wit-parser can resolve the
// cross-package reference natively — no vendored deps/ tree needed.
// World name is fully qualified.
wasmtime::component::bindgen!({
    path: ["../../wit/nexum-host", "../../wit/shepherd-cow"],
    world: "shepherd:cow/shepherd",
    imports: { default: async },
    exports: { default: async },
});

use nexum::host::types::HostErrorKind;

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    /// Origin for `clock::monotonic-ns`. Differences between successive
    /// readings are the only meaningful values.
    monotonic_baseline: Instant,
    /// Per-module `[capabilities.http].allow` allowlist (from nexum.toml).
    /// Consulted by `http::fetch` before any outbound call.
    http_allowlist: Vec<String>,
    /// Namespace for the running module's `local-store` rows. Set from
    /// `manifest.module.name` at instantiation.
    module_namespace: String,
    /// `cow-api` backend — per-chain `OrderBookApi` clients + reqwest.
    cow: host::cow_orderbook::OrderBookPool,
    /// `chain` backend — per-chain alloy `DynProvider` pool.
    chain: host::provider_pool::ProviderPool,
    /// `local-store` backend — redb file with host-side namespacing.
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
            Err(host::cow_orderbook::CowApiError::Orderbook(msg)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::Denied,
                code: 0,
                message: msg,
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
        // No keystore wired yet — return an empty roster so guests can
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
        // Empty result — same posture as `identity::accounts`.
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
        // return zero-filled bytes — guests that need a strong-failure
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
        // every request — matching the "no implicit network" stance.
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
                     add it to nexum.toml to permit"
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
    let cli = Cli::parse();
    let wasm_path = cli.wasm_path;
    let explicit_manifest = cli.manifest_path;
    let explicit_engine_config = cli.engine_config_path;

    // -- 1. Load engine config (optional). --
    let engine_cfg = engine_config::load_or_default(explicit_engine_config.as_deref())?;

    // -- 2. Install tracing subscriber. --
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&engine_cfg.engine.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();

    info!("nexum-engine starting");
    info!(wasm = %wasm_path.display(), "loading component");

    // -- 3. Load the module manifest. --
    let manifest_path =
        explicit_manifest.or_else(|| wasm_path.parent().map(|p| p.join("nexum.toml")));
    let loaded = match manifest_path.as_deref() {
        Some(p) if p.exists() => {
            info!(manifest = %p.display(), "loading nexum.toml");
            manifest::load(p)?
        }
        _ => manifest::fallback_manifest(),
    };

    // -- 4. Bring up the host backends. --
    std::fs::create_dir_all(&engine_cfg.engine.state_dir).with_context(|| {
        format!(
            "create state directory {}",
            engine_cfg.engine.state_dir.display()
        )
    })?;
    let store_path = engine_cfg.engine.state_dir.join("local-store.redb");
    let local_store = host::local_store_redb::LocalStore::open(&store_path)
        .with_context(|| format!("open local-store at {}", store_path.display()))?;
    let cow_pool = host::cow_orderbook::OrderBookPool::with_default_chains();
    let provider_pool = host::provider_pool::ProviderPool::from_config(&engine_cfg)
        .await
        .context("open chain providers")?;

    // -- 5. Build the wasmtime engine + component. --
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    // `async_support` was deprecated in wasmtime 45 — the engine
    // resolves async on its own. Keeping the call out of the Config
    // chain silences the `deprecated` warning under
    // `RUSTFLAGS=-D warnings`.
    let engine = Engine::new(&config)?;

    let load_start = Instant::now();
    let component =
        Component::from_file(&engine, &wasm_path).context("failed to load component")?;
    tracing::debug!(elapsed_ms = ?load_start.elapsed(), "component load");

    let mut linker = Linker::<HostState>::new(&engine);
    Shepherd::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state| state,
    )?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    let wasi = WasiCtxBuilder::new().inherit_stdio().build();
    let module_namespace = if loaded.manifest.module.name.is_empty() {
        "module".to_owned()
    } else {
        loaded.manifest.module.name.clone()
    };

    let mut store = Store::new(
        &engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            monotonic_baseline: Instant::now(),
            http_allowlist: loaded.http_allowlist,
            module_namespace,
            cow: cow_pool,
            chain: provider_pool,
            store: local_store,
        },
    );

    let inst_start = Instant::now();
    let bindings = Shepherd::instantiate_async(&mut store, &component, &linker)
        .await
        .context("failed to instantiate component")?;
    tracing::debug!(elapsed_ms = ?inst_start.elapsed(), "component instantiate");

    info!("calling init");
    let config_entries: Config = if loaded.config.is_empty() {
        vec![("name".into(), loaded.manifest.module.name.clone())]
    } else {
        loaded.config
    };
    let init_start = Instant::now();
    match bindings.call_init(&mut store, &config_entries).await? {
        Ok(()) => info!(elapsed_ms = ?init_start.elapsed(), "init succeeded"),
        Err(e) => warn!(
            domain = %e.domain,
            kind = ?e.kind,
            code = e.code,
            message = %e.message,
            "init failed",
        ),
    }

    // Dispatch a test block event (timestamps are ms since Unix epoch, UTC).
    info!("dispatching test block event");
    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    };
    let event = nexum::host::types::Event::Block(block);
    let evt_start = Instant::now();
    match bindings.call_on_event(&mut store, &event).await? {
        Ok(()) => info!(elapsed_ms = ?evt_start.elapsed(), "on-event succeeded"),
        Err(e) => warn!(
            domain = %e.domain,
            kind = ?e.kind,
            code = e.code,
            message = %e.message,
            "on-event failed",
        ),
    }

    info!("done");
    Ok(())
}
