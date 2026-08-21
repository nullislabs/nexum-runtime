//! Store and linker construction: one wasmtime `Store` per run.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use tracing::warn;
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{HostMonotonicClock, HostWallClock, WasiCtxBuilder};

use super::Shared;
use crate::bindings::TriggerModule;
use crate::engine_config::{LogBoundsPolicy, OutboundHttpLimits, PolicyCeilings};
use crate::error::{EngineRefusal, RuntimeError};
use crate::manifest::ResourceSection;
use nexum_primitives::host_pattern::HostPattern;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::Extension;
use nexum_runtime_api::{RuntimeTypes, StateHandle, StateStore};
use nexum_runtime_http::HttpGate;
use nexum_runtime_logs::{LogBounds, LogChannel, RunId, StdioStream};
use nexum_runtime_wasm::HostState;

pub(super) type HostStore<T> = Store<HostState<T>>;

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

/// `[component.resources]` layered under the `[policy]` ceilings.
#[derive(Clone, Copy)]
pub(super) struct ResolvedLimits {
    pub(super) fuel: u64,
    pub(super) memory: usize,
    pub(super) state_bytes: u64,
}

/// Unset `[component.resources]` fields keep the component's `[policy]`
/// ceiling; a set field narrows and never widens.
///
/// The manifest is author-supplied, so the policy value is a ceiling rather
/// than a default. See `docs/adr/0001-operator-config-separate-and-trusted.md`.
pub(super) fn resolve_module_limits(
    id: &str,
    res: &ResourceSection,
    ceilings: &PolicyCeilings,
) -> ResolvedLimits {
    ResolvedLimits {
        fuel: clamp(
            id,
            "max_fuel_per_dispatch",
            res.max_fuel_per_dispatch,
            ceilings.max_fuel_per_dispatch.get(),
        ),
        memory: clamp(
            id,
            "max_memory_bytes",
            res.max_memory_bytes,
            ceilings.max_memory_bytes.get(),
        ),
        state_bytes: clamp(
            id,
            "max_state_bytes",
            res.max_state_bytes,
            ceilings.max_state_bytes,
        ),
    }
}

/// The engine value unless the manifest asks for less. A request above the
/// ceiling is capped and logged: handing back a smaller budget than the
/// manifest declares would otherwise look like the module misbehaving.
fn clamp<T: Ord + std::fmt::Display>(id: &str, field: &str, requested: Option<T>, ceiling: T) -> T {
    match requested {
        Some(value) if value > ceiling => {
            warn!(
                target: "manifest",
                id,
                field,
                requested = %value,
                ceiling = %ceiling,
                "[component.resources] exceeds the engine ceiling; using the ceiling",
            );
            ceiling
        }
        Some(value) => value,
        None => ceiling,
    }
}

/// Cached whole for restarts, so a rebuilt store is budgeted exactly like
/// the boot-time one.
pub(super) struct StoreSpec {
    pub(super) http_allowlist: Vec<HostPattern>,
    /// `[policy.component.<id>].http_allow`; the gate intersects it with
    /// the manifest list.
    pub(super) http_operator_allow: Option<Vec<HostPattern>>,
    pub(super) http_limits: OutboundHttpLimits,
    /// Operator-permitted addresses that would otherwise be refused.
    pub(super) http_permitted: Vec<std::net::IpAddr>,
    /// `[policy].http_deny` ranges, refused after every allowlist.
    pub(super) http_denied: Vec<ipnet::IpNet>,
    pub(super) memory_limit: usize,
    pub(super) fuel: u64,
    pub(super) chain_response_max_bytes: usize,
    pub(super) state_quota: u64,
    /// Admission bounds; a restart mints a fresh bucket, so this is per
    /// run, not per module lifetime.
    pub(super) log_bounds: LogBoundsPolicy,
}

/// Mints the run identity for `name` at `seq` and builds its store.
pub(super) fn fresh_run_store<T: RuntimeTypes>(
    shared: &Shared<T>,
    name: &ModuleId,
    seq: u64,
    spec: &StoreSpec,
) -> Result<(RunId, HostStore<T>)> {
    let run = RunId::new(name.clone(), seq);
    let store = build(shared, spec, run.clone())?;
    Ok((run, store))
}

fn build<T: RuntimeTypes>(
    shared: &Shared<T>,
    spec: &StoreSpec,
    run: RunId,
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
            LogChannel::Stdout,
        ))
        .stderr(StdioStream::new(
            router.clone(),
            run.clone(),
            LogChannel::Stderr,
        ));
    if let Some(clocks) = &shared.clocks {
        builder.wall_clock(SharedWallClock(clocks.wall()));
        builder.monotonic_clock(SharedMonotonicClock(clocks.monotonic()));
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
            http_gate: HttpGate::new(
                namespace,
                spec.http_allowlist.clone(),
                spec.http_operator_allow.clone(),
                spec.http_limits,
                spec.http_permitted.clone(),
                spec.http_denied.clone(),
            ),
            run,
            log_router: router,
            log_bounds: LogBounds::new(spec.log_bounds, std::time::Instant::now()),
            chain: shared.components.chain.clone(),
            chain_response_max_bytes: spec.chain_response_max_bytes,
            store: module_store,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(spec.fuel)?;
    Ok(store)
}

/// The wasmtime config every engine, launch and test alike, is built from.
pub(crate) fn wasmtime_config() -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config
}

/// Build the shared engine every module instantiates against.
pub fn engine() -> Result<wasmtime::Engine, crate::error::RuntimeError> {
    wasmtime::Engine::new(&wasmtime_config())
        .map_err(crate::error::EngineRefusal::new)
        .map_err(Into::into)
}

/// The same `extensions` slice must drive this and capability enforcement:
/// an import instantiates only if that extension's hook is linked.
pub fn build_linker<T: RuntimeTypes<State = HostState<T>>>(
    engine: &Engine,
    extensions: &[Arc<dyn Extension<T>>],
) -> Result<Linker<HostState<T>>, RuntimeError> {
    let mut linker = Linker::<HostState<T>>::new(engine);
    TriggerModule::add_to_linker::<HostState<T>, HasSelf<HostState<T>>>(&mut linker, |state| state)
        .map_err(EngineRefusal::new)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(EngineRefusal::new)?;
    // wasi:http only; the p2 call above already covers the shared
    // wasi:io/wasi:clocks interfaces.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(EngineRefusal::new)?;
    for ext in extensions {
        ext.link(&mut linker)?;
    }
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::WasiClockOverride;
    use crate::test_utils::ManualClock;

    /// [`build`] serves the guest `clocks.wall`; the extension seam hands out
    /// that same handle, not a second clock over the same source.
    #[test]
    fn the_effective_wall_clock_is_the_handle_the_guest_store_serves() {
        let clocks = ManualClock::new().as_override();
        let served = WasiClockOverride::effective_wall(Some(&clocks));
        assert!(Arc::ptr_eq(&clocks.wall(), &served));
    }
}
