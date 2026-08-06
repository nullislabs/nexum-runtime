//! Multi-module supervisor: loads `engine.toml` entries, one wasmtime `Store`
//! each, and routes subscribed events.

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
use admission::{ProviderKinds, capability_registry, enforce_extension_uniqueness, provider_kinds};
use cursors::ChainLogCursors;
use load::{LoadedModule, LoadedProvider};
use prepass::{
    MODULE_FALLBACK_NAME, enforce_configured_chains, load_required_manifest, manifest_namespace,
};

/// Owns every loaded module and provider and exposes the dispatch surface.
pub struct Supervisor<T: RuntimeTypes> {
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    providers: Vec<LoadedProvider>,
    /// Poison-pill thresholds resolved from `[limits.poison]` at boot.
    policy: PoisonPolicy,
    /// In-memory mirror of the persisted chain-log cursors.
    chain_log_cursors: ChainLogCursors,
}

/// Boot inputs derived from [`EngineConfig`], bundled once at the call site.
pub struct BootEnv<'a> {
    pub limits: &'a ModuleLimits,
    pub configured_chains: ConfiguredChains,
    pub require_component_digest: bool,
}

impl<'a> BootEnv<'a> {
    pub fn from_config(cfg: &'a EngineConfig) -> Self {
        Self {
            limits: &cfg.limits,
            configured_chains: ConfiguredChains::from_config(cfg),
            require_component_digest: cfg.engine.require_component_digest,
        }
    }
}

/// Cached at boot so restarts rebuild an identical store and linker.
pub(super) struct Shared<T: RuntimeTypes> {
    pub(super) engine: Engine,
    pub(super) components: Components<T>,
    /// The same slice drives admission, linking, and capability enforcement.
    pub(super) extensions: Vec<Arc<dyn Extension<T>>>,
    /// Built once; carried by every module store.
    pub(super) services: HostServices,
    pub(super) kinds: ProviderKinds<T>,
    /// Applied to every store; `None` leaves the ambient host clocks.
    pub(super) clocks: Option<WasiClockOverride>,
}

impl<T: RuntimeTypes> Supervisor<T> {
    pub async fn boot(
        engine: &Engine,
        linker: &Linker<HostState<T>>,
        engine_cfg: &EngineConfig,
        components: &Components<T>,
        extensions: &[Arc<dyn Extension<T>>],
        clocks: Option<WasiClockOverride>,
    ) -> Result<Self> {
        let shared = wire_extensions(engine, components, extensions, clocks, true)?;
        let registry = capability_registry(&shared.extensions);
        let prepass = prepass::run(engine_cfg, &registry)?;
        // Providers boot first, so every module store built after already
        // routes to the installed instances.
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

    /// Single-component boot for `just run` without an `engine.toml`.
    pub async fn boot_single(
        engine: &Engine,
        linker: &Linker<HostState<T>>,
        entry: &ModuleEntry,
        components: &Components<T>,
        env: &BootEnv<'_>,
        extensions: &[Arc<dyn Extension<T>>],
        clocks: Option<WasiClockOverride>,
    ) -> Result<Self> {
        // Provider kinds come only from `engine.toml`, so none register here.
        let shared = wire_extensions(engine, components, extensions, clocks, false)?;
        let registry = capability_registry(&shared.extensions);
        let loaded_manifest =
            load_required_manifest(&entry.path, entry.manifest.as_deref(), &registry, "module")?;
        enforce_configured_chains(
            &manifest_namespace(&loaded_manifest, MODULE_FALLBACK_NAME),
            &loaded_manifest,
            &env.configured_chains,
        )?;
        let loaded = load::module(
            &shared,
            linker,
            entry,
            loaded_manifest,
            env.limits,
            env.require_component_digest,
            &[],
        )
        .await?;
        Ok(Self {
            shared,
            modules: vec![loaded],
            providers: Vec::new(),
            policy: env.limits.poison(),
            chain_log_cursors: ChainLogCursors::default(),
        })
    }

    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Alive or not.
    pub fn adapter_count(&self) -> usize {
        self.providers.len()
    }

    /// Excludes init-failed (permanent) and in-backoff modules.
    pub fn alive_count(&self) -> usize {
        self.modules
            .iter()
            .filter(|m| m.health.dispatchable())
            .count()
    }

    /// Distinguishes benign "no subscriptions declared" from "every declared
    /// subscription belongs to a dead module" (operator error).
    pub fn dead_modules_hold_subscriptions(&self) -> bool {
        self.modules
            .iter()
            .any(|m| !m.health.dispatchable() && !m.subscriptions.is_empty())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn poisoned_count(&self) -> usize {
        self.modules
            .iter()
            .filter(|m| m.health.is_poisoned())
            .count()
    }

    pub fn services(&self) -> &HostServices {
        &self.shared.services
    }
}

/// The resulting [`Shared`] is the one wiring every later phase reads.
/// `with_provider_kinds: false` skips [`provider_kinds`], which refuses a serviceless kind.
fn wire_extensions<T: RuntimeTypes>(
    engine: &Engine,
    components: &Components<T>,
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<WasiClockOverride>,
    with_provider_kinds: bool,
) -> Result<Shared<T>> {
    enforce_extension_uniqueness(extensions)?;
    let services = HostServices::from_extensions(extensions)?;
    let kinds = if with_provider_kinds {
        provider_kinds(extensions, &services)?
    } else {
        ProviderKinds::new()
    };
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

/// The providers' manifests as the worker install predicates see them.
fn project_manifests(providers: &[LoadedProvider]) -> Vec<ProviderManifest> {
    providers
        .iter()
        .map(|p| ProviderManifest {
            name: p.name.to_string(),
            kind: p.kind,
            sections: p.sections.clone(),
            component_digest: p.seed.artifact.digest,
        })
        .collect()
}

/// In declaration order, against the installed providers.
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

fn assemble<T: RuntimeTypes>(
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    providers: Vec<LoadedProvider>,
    policy: PoisonPolicy,
) -> Supervisor<T> {
    let alive = modules.iter().filter(|m| m.health.dispatchable()).count();
    let adapters_alive = providers.iter().filter(|p| p.health.dispatchable()).count();
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
pub(crate) mod tests;
