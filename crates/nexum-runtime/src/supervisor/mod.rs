//! Multi-module supervisor.
//!
//! Loads every `[[modules]]` and `[[adapters]]` entry from `engine.toml`,
//! instantiates each against a dedicated wasmtime `Store`, and routes
//! subscribed events. A trap marks a component dead with a backoff; a
//! failed `init` at boot is permanently dead. Providers ride the same
//! sweeps via a shared [`Liveness`](crate::host::actor::Liveness).

mod admission;
mod artifact;
mod cursors;
mod dispatch;
mod lifecycle;
mod load;
mod prepass;
mod store;
mod subscriptions;

pub use prepass::ConfiguredChains;
pub use store::{WasiClockOverride, build_linker, build_provider_linker};
pub use subscriptions::ChainLogSub;

pub(crate) use admission::capability_registry;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::engine_config::{EngineConfig, ModuleEntry, ModuleLimits};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::extension::{Extension, HostServices, ProviderManifest};
use crate::host::state::HostState;
use crate::runtime::poison_policy::PoisonPolicy;
use admission::{ProviderKinds, enforce_extension_uniqueness, provider_kinds};
use cursors::ChainLogCursors;
use load::{LoadedModule, LoadedProvider};
use prepass::{
    MODULE_FALLBACK_NAME, enforce_configured_chains, load_required_manifest, manifest_namespace,
};

/// Owns every loaded module and provider and exposes the dispatch surface.
/// Generic over the [`RuntimeTypes`] backend lattice.
pub struct Supervisor<T: RuntimeTypes> {
    /// Backends and boot-time wiring every load, restart, and dispatch
    /// path shares.
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    /// Providers loaded at boot; swept for restart and poison alongside
    /// the modules.
    providers: Vec<LoadedProvider>,
    /// Poison-pill thresholds resolved from `[limits.poison]` at boot.
    policy: PoisonPolicy,
    /// In-memory mirror of the persisted chain-log cursors.
    chain_log_cursors: ChainLogCursors,
}

/// The shared backends: cached at boot so restarts rebuild an identical
/// store and linker without re-reading configuration.
pub(super) struct Shared<T: RuntimeTypes> {
    pub(super) engine: Engine,
    pub(super) components: Components<T>,
    /// Extensions wired at boot; the same slice drives admission, linking,
    /// and capability enforcement.
    pub(super) extensions: Vec<Arc<dyn Extension<T>>>,
    /// Extension-owned host services, built once and carried by every
    /// module store.
    pub(super) services: HostServices,
    /// Registered provider kinds paired with their services, for the
    /// restart sweep to reinstall through.
    pub(super) kinds: ProviderKinds<T>,
    /// Optional WASI clock override applied to every store. `None` leaves
    /// the ambient host clocks.
    pub(super) clocks: Option<WasiClockOverride>,
}

impl<T: RuntimeTypes> Supervisor<T> {
    /// Compile and instantiate every module and provider in `engine_cfg`.
    /// The `Engine` and `Linker` are passed in.
    pub async fn boot(
        engine: &Engine,
        linker: &Linker<HostState<T>>,
        engine_cfg: &EngineConfig,
        components: &Components<T>,
        extensions: &[Arc<dyn Extension<T>>],
        clocks: Option<WasiClockOverride>,
    ) -> Result<Self> {
        let shared = wire_extensions(engine, components, extensions, clocks)?;
        let registry = capability_registry(&shared.extensions);
        let prepass = prepass::run(engine_cfg, &registry)?;
        // Providers boot first into their extension-owned services, so
        // every module store built after already routes to the installed
        // instances.
        let providers = load_providers(&shared, engine_cfg, prepass.adapter_manifests).await?;
        let provider_manifests = project_manifests(&providers);
        let modules = load_modules(
            &shared,
            linker,
            engine_cfg,
            prepass.module_manifests,
            &provider_manifests,
        )
        .await?;
        Ok(assemble(
            shared,
            modules,
            providers,
            engine_cfg.limits.poison(),
        ))
    }

    /// Construct from a single `(component, manifest)` pair, for `just run`
    /// without an `engine.toml`.
    // One flat argument per shared backend and resource knob, plus the
    // optional clock override; bundling would obscure the call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn boot_single(
        engine: &Engine,
        linker: &Linker<HostState<T>>,
        wasm: &Path,
        manifest: Option<&Path>,
        components: &Components<T>,
        limits: &ModuleLimits,
        configured_chains: &ConfiguredChains,
        require_component_digest: bool,
        extensions: &[Arc<dyn Extension<T>>],
        clocks: Option<WasiClockOverride>,
    ) -> Result<Self> {
        enforce_extension_uniqueness(extensions)?;
        let services = HostServices::from_extensions(extensions)?;
        // The single-module override path serves `just run`; providers are
        // configured through `engine.toml`, so no kinds register here.
        let shared = Shared {
            engine: engine.clone(),
            components: components.clone(),
            extensions: extensions.to_vec(),
            services,
            kinds: ProviderKinds::new(),
            clocks,
        };
        let registry = capability_registry(&shared.extensions);
        let entry = ModuleEntry {
            path: wasm.to_path_buf(),
            manifest: manifest.map(Path::to_path_buf),
        };
        let loaded_manifest =
            load_required_manifest(&entry.path, entry.manifest.as_deref(), &registry, "module")?;
        enforce_configured_chains(
            &manifest_namespace(&loaded_manifest, MODULE_FALLBACK_NAME),
            &loaded_manifest,
            configured_chains,
        )?;
        let loaded = load::module(
            &shared,
            linker,
            &entry,
            loaded_manifest,
            limits,
            require_component_digest,
            &[],
        )
        .await?;
        Ok(Self {
            shared,
            modules: vec![loaded],
            providers: Vec::new(),
            policy: limits.poison(),
            chain_log_cursors: ChainLogCursors::default(),
        })
    }

    /// Number of modules currently loaded.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Number of providers loaded at boot, alive or not.
    pub fn adapter_count(&self) -> usize {
        self.providers.len()
    }

    /// Modules currently alive. Not alive when `init` returned `Err`
    /// (permanent) or a trap's backoff has not elapsed.
    pub fn alive_count(&self) -> usize {
        self.modules.iter().filter(|m| m.alive).count()
    }

    /// True when an init-failed module declared subscriptions. Lets the
    /// launch path tell "no subscriptions declared" (benign) from "every
    /// declared subscription belongs to a dead module" (operator error).
    pub fn dead_modules_hold_subscriptions(&self) -> bool {
        self.modules
            .iter()
            .any(|m| !m.alive && !m.subscriptions.is_empty())
    }

    /// Modules currently poisoned.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn poisoned_count(&self) -> usize {
        self.modules.iter().filter(|m| m.poisoned).count()
    }

    /// The extension-owned services, shared by every module store.
    pub fn services(&self) -> &HostServices {
        &self.shared.services
    }
}

/// Enforce extension uniqueness, build the shared services, and register
/// the provider kinds; the resulting [`Shared`] is the one wiring every
/// later phase reads.
fn wire_extensions<T: RuntimeTypes>(
    engine: &Engine,
    components: &Components<T>,
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<WasiClockOverride>,
) -> Result<Shared<T>> {
    enforce_extension_uniqueness(extensions)?;
    let services = HostServices::from_extensions(extensions)?;
    let kinds = provider_kinds(extensions, &services)?;
    Ok(Shared {
        engine: engine.clone(),
        components: components.clone(),
        extensions: extensions.to_vec(),
        services,
        kinds,
        clocks,
    })
}

/// Load every `[[adapters]]` entry, in declaration order.
async fn load_providers<T: RuntimeTypes>(
    shared: &Shared<T>,
    engine_cfg: &EngineConfig,
    manifests: Vec<crate::manifest::LoadedManifest>,
) -> Result<Vec<LoadedProvider>> {
    let mut providers = Vec::with_capacity(engine_cfg.adapters.len());
    for (entry, loaded_manifest) in engine_cfg.adapters.iter().zip(manifests) {
        let loaded = load::provider(
            shared,
            entry,
            loaded_manifest,
            &engine_cfg.limits,
            engine_cfg.engine.require_component_digest,
        )
        .await
        .with_context(|| format!("load provider {}", entry.path.display()))?;
        providers.push(loaded);
    }
    Ok(providers)
}

/// The loaded providers' manifests, as the worker install predicates see
/// them.
fn project_manifests(providers: &[LoadedProvider]) -> Vec<ProviderManifest> {
    providers
        .iter()
        .map(|p| ProviderManifest {
            name: p.name.clone(),
            kind: p.kind,
            sections: p.sections.clone(),
            component_digest: p.seed.artifact.digest,
        })
        .collect()
}

/// Load every `[[modules]]` entry against the installed providers, in
/// declaration order.
async fn load_modules<T: RuntimeTypes>(
    shared: &Shared<T>,
    linker: &Linker<HostState<T>>,
    engine_cfg: &EngineConfig,
    manifests: Vec<crate::manifest::LoadedManifest>,
    provider_manifests: &[ProviderManifest],
) -> Result<Vec<LoadedModule<T>>> {
    let mut modules = Vec::with_capacity(engine_cfg.modules.len());
    for (entry, loaded_manifest) in engine_cfg.modules.iter().zip(manifests) {
        let loaded = load::module(
            shared,
            linker,
            entry,
            loaded_manifest,
            &engine_cfg.limits,
            engine_cfg.engine.require_component_digest,
            provider_manifests,
        )
        .await
        .with_context(|| format!("load module {}", entry.path.display()))?;
        modules.push(loaded);
    }
    Ok(modules)
}

/// Assemble the booted supervisor and log the loaded and alive tallies.
fn assemble<T: RuntimeTypes>(
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    providers: Vec<LoadedProvider>,
    policy: PoisonPolicy,
) -> Supervisor<T> {
    let alive = modules.iter().filter(|m| m.alive).count();
    let adapters_alive = providers.iter().filter(|p| p.alive).count();
    info!(
        loaded = modules.len(),
        alive,
        adapters = providers.len(),
        adapters_alive,
        "supervisor up"
    );
    Supervisor {
        shared,
        modules,
        providers,
        policy,
        chain_log_cursors: ChainLogCursors::default(),
    }
}

/// Core-only lattice for the runtime's own tests (`Ext = ()`).
#[cfg(test)]
#[derive(Clone, Copy, Default)]
pub(crate) struct TestTypes;

#[cfg(test)]
impl crate::sealed::SealedRuntimeTypes for TestTypes {}

#[cfg(test)]
impl RuntimeTypes for TestTypes {
    type Store = crate::host::local_store_redb::LocalStore;
    type Ext = ();
}

/// The supervisor the runtime's own tests drive.
#[cfg(test)]
pub(crate) type DefaultSupervisor = Supervisor<TestTypes>;

#[cfg(test)]
use admission::enforce_extension_sections;
#[cfg(test)]
use artifact::read_verified_component;
#[cfg(test)]
use cursors::{chainlog_cursor_key, commit_chain_log_cursor, progress_key, read_chain_log_cursor};
#[cfg(test)]
use dispatch::with_dispatch_deadline;
#[cfg(test)]
use prepass::{NamespaceLedger, claim_namespace, unconfigured_chain};
#[cfg(test)]
use store::resolve_module_limits;
#[cfg(test)]
use subscriptions::build_alloy_filter;

#[cfg(test)]
use crate::bindings::nexum;
#[cfg(test)]
use crate::digest::{ContentDigest, DigestMismatch};
#[cfg(test)]
use crate::host::extension::{HostService, Installed, ProviderInstance, ProviderKind};
#[cfg(test)]
use crate::host::logs::LogSource;
#[cfg(test)]
use crate::host::provider_pool::ProviderPool;
#[cfg(test)]
use crate::manifest::{self, CapabilityRegistry};
#[cfg(test)]
use alloy_chains::Chain;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use tracing_core::Level;

#[cfg(test)]
mod tests;
