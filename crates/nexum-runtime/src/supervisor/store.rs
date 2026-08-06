//! Store and linker construction: one wasmtime `Store` per run.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{HostMonotonicClock, HostWallClock, WasiCtxBuilder};

use super::Shared;
use crate::bindings::EventModule;
use crate::engine_config::{ModuleLimits, OutboundHttpLimits};
use crate::host::component::{RuntimeTypes, StateHandle, StateStore};
use crate::host::extension::{Extension, HostServices, ProviderKind};
use crate::host::http::HttpGate;
use crate::host::logs::{LogSource, RunId, StdioStream};
use crate::host::state::HostState;
use crate::manifest::ResourceSection;

pub(super) type HostStore<T> = Store<HostState<T>>;

/// Shared sources let a test drive guest-visible time and the wall clock
/// extensions receive; `None` keeps the ambient clocks. `RunId.started_at`
/// is host wall-clock and unaffected.
#[derive(Clone)]
pub struct WasiClockOverride {
    pub(super) wall: Arc<dyn HostWallClock + Send + Sync>,
    pub(super) monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
}

impl WasiClockOverride {
    pub fn new(
        wall: Arc<dyn HostWallClock + Send + Sync>,
        monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
    ) -> Self {
        Self { wall, monotonic }
    }

    /// The effective host wall clock: the override's wall clock when set,
    /// else the real host clock.
    pub fn effective_wall(clocks: Option<&Self>) -> Arc<dyn HostWallClock + Send + Sync> {
        match clocks {
            Some(clocks) => clocks.wall.clone(),
            None => Arc::new(wasmtime_wasi::clocks::WallClock::default()),
        }
    }
}

struct SharedWallClock(Arc<dyn HostWallClock + Send + Sync>);

impl HostWallClock for SharedWallClock {
    fn resolution(&self) -> std::time::Duration {
        self.0.resolution()
    }

    fn now(&self) -> std::time::Duration {
        self.0.now()
    }
}

struct SharedMonotonicClock(Arc<dyn HostMonotonicClock + Send + Sync>);

impl HostMonotonicClock for SharedMonotonicClock {
    fn resolution(&self) -> u64 {
        self.0.resolution()
    }

    fn now(&self) -> u64 {
        self.0.now()
    }
}

/// `[module.resources]` layered over engine `[limits]`.
pub(super) struct ResolvedLimits {
    pub(super) fuel: u64,
    pub(super) memory: usize,
    pub(super) state_bytes: u64,
}

/// Unset `[module.resources]` fields keep the engine `[limits]` default.
pub(super) fn resolve_module_limits(res: &ResourceSection, cfg: &ModuleLimits) -> ResolvedLimits {
    ResolvedLimits {
        fuel: res.max_fuel_per_event.unwrap_or(cfg.fuel()),
        memory: res.max_memory_bytes.unwrap_or(cfg.memory()),
        state_bytes: res.max_state_bytes.unwrap_or(cfg.state_bytes()),
    }
}

/// Cached whole for restarts, so a rebuilt store is budgeted exactly like
/// the boot-time one.
pub(super) struct StoreSpec {
    pub(super) http_allowlist: Vec<String>,
    pub(super) http_limits: OutboundHttpLimits,
    pub(super) messaging_topics: Vec<String>,
    pub(super) memory_limit: usize,
    pub(super) fuel: u64,
    pub(super) chain_response_max_bytes: usize,
    pub(super) state_quota: u64,
}

/// Takes a freshly minted [`RunId`]; `services` is the module map, empty
/// for a provider store.
pub(super) fn build<T: RuntimeTypes>(
    shared: &Shared<T>,
    spec: &StoreSpec,
    run: RunId,
    services: HostServices,
) -> Result<HostStore<T>> {
    let namespace: &str = run.module.as_str();
    // Stdio is captured as tagged log records, stdin stays closed; the ctx
    // grants no network, so the allowlisted wasi:http gate is the only live path.
    let router = shared.components.logs.router();
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdout(StdioStream::new(
            router.clone(),
            run.clone(),
            LogSource::Stdout,
        ))
        .stderr(StdioStream::new(
            router.clone(),
            run.clone(),
            LogSource::Stderr,
        ));
    if let Some(clocks) = &shared.clocks {
        builder.wall_clock(SharedWallClock(clocks.wall.clone()));
        builder.monotonic_clock(SharedMonotonicClock(clocks.monotonic.clone()));
    }
    let wasi = builder.build();
    let limits = wasmtime::StoreLimitsBuilder::new()
        .memory_size(spec.memory_limit)
        .build();
    let module_store = shared
        .components
        .store
        .module(namespace)
        .map_err(|e| anyhow!("local-store namespace for {namespace}: {e}"))?
        .with_quota(spec.state_quota);
    let mut store = Store::new(
        &shared.engine,
        HostState {
            wasi,
            table: ResourceTable::new(),
            limits,
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_gate: HttpGate::new(namespace, spec.http_allowlist.clone(), spec.http_limits),
            messaging_topics: spec.messaging_topics.clone(),
            run,
            log_router: router,
            ext: shared.components.ext.clone(),
            chain: shared.components.chain.clone(),
            chain_response_max_bytes: spec.chain_response_max_bytes,
            // Provider guests never reach this: `build_provider_linker`
            // links only `kind.link` plus WASI.
            store: module_store,
            services,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(spec.fuel)?;
    Ok(store)
}

/// The same `extensions` slice must drive this and capability enforcement:
/// an import instantiates only if that extension's hook is linked.
pub fn build_linker<T: RuntimeTypes>(
    engine: &Engine,
    extensions: &[Arc<dyn Extension<T>>],
) -> anyhow::Result<Linker<HostState<T>>> {
    let mut linker = Linker::<HostState<T>>::new(engine);
    EventModule::add_to_linker::<HostState<T>, HasSelf<HostState<T>>>(&mut linker, |state| state)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    // wasi:http only; the p2 call above already covers the shared
    // wasi:io/wasi:clocks interfaces.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    for ext in extensions {
        ext.link(&mut linker)?;
    }
    Ok(linker)
}

/// Core `nexum:host` interfaces are withheld, so a provider importing one
/// fails to instantiate; extensions are never linked into providers.
pub fn build_provider_linker<T: RuntimeTypes>(
    engine: &Engine,
    kind: &dyn ProviderKind<T>,
) -> anyhow::Result<Linker<HostState<T>>> {
    let mut linker = Linker::<HostState<T>>::new(engine);
    kind.link(&mut linker)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Ok(linker)
}
