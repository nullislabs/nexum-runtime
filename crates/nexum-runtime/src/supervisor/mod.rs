//! Multi-module supervisor: loads `engine.toml` entries, one wasmtime `Store`
//! each, and routes subscribed events.

mod admission;
mod artifact;
mod cursors;
mod dispatch;
mod lifecycle;
pub(crate) mod load;
pub(crate) mod prepass;
mod role;
mod store;
mod subscriptions;

pub use prepass::ConfiguredChains;
pub use store::{WasiClockOverride, build_linker, build_provider_linker};
pub use subscriptions::{ChainLogSub, SubscriptionPlan, Viability};

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::digest::DigestMismatch;
use crate::engine_config::{EngineConfig, ModuleEntry, ModuleLimits};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::extension::{Extension, HostServices, ServiceManifest};
use crate::host::state::HostState;
use crate::manifest::CapabilityError;
use crate::runtime::poison_policy::PoisonPolicy;
use admission::{ServiceKinds, capability_registry, enforce_extension_uniqueness, provider_kinds};
use cursors::ChainLogCursors;
use load::{LoadRefusal, LoadedModule, LoadedProvider};
use prepass::{BootRefusal, enforce_subscriptions, load_required_manifest, manifest_namespace};
use role::Role;

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
    pub(super) kinds: ServiceKinds<T>,
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
        let booted: Result<Self> = async {
            let shared = wire_extensions(engine, components, extensions, clocks, true)?;
            let registry = capability_registry(&shared.extensions);
            let prepass = prepass::run(engine_cfg, &registry)?;
            // Providers boot first, so every module store built after already
            // routes to the installed instances.
            let providers = load_role(
                &engine_cfg.services,
                prepass.adapter_manifests,
                Role::Service,
                |e| &e.path,
                async |entry, manifest| {
                    load::provider(
                        &shared,
                        entry,
                        manifest,
                        &engine_cfg.limits,
                        engine_cfg.engine.require_component_digest,
                    )
                    .await
                },
            )
            .await?;
            let provider_manifests = project_manifests(&providers);
            let modules = load_role(
                &engine_cfg.modules,
                prepass.module_manifests,
                Role::Module,
                |e| &e.path,
                async |entry, manifest| {
                    load::module(
                        &shared,
                        linker,
                        entry,
                        manifest,
                        &engine_cfg.limits,
                        engine_cfg.engine.require_component_digest,
                        &provider_manifests,
                    )
                    .await
                },
            )
            .await?;
            Ok(assemble(
                shared,
                modules,
                providers,
                engine_cfg.limits.poison(),
            ))
        }
        .await;
        booted.inspect_err(count_boot_refusal)
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
        let booted: Result<Self> = async {
            // Provider kinds come only from `engine.toml`, so none register here.
            let shared = wire_extensions(engine, components, extensions, clocks, false)?;
            let registry = capability_registry(&shared.extensions);
            let loaded_manifest = load_required_manifest(
                &entry.path,
                entry.manifest.as_deref(),
                &registry,
                Role::Module.label(),
            )?;
            enforce_subscriptions(
                Role::Module,
                &manifest_namespace(&loaded_manifest),
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
        .await;
        booted.inspect_err(count_boot_refusal)
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

/// Counts a refusal under its typed kind; wraps without a typed root go uncounted.
fn count_boot_refusal(err: &anyhow::Error) {
    let Some(kind) = boot_refusal_kind(err) else {
        return;
    };
    metrics::counter!("nexum_runtime_boot_refusals_total", "error_kind" => kind).increment(1);
}

/// A root missing here is a refusal class the counter never sees.
fn boot_refusal_kind(err: &anyhow::Error) -> Option<&'static str> {
    err.chain().find_map(|cause| {
        if let Some(refusal) = cause.downcast_ref::<BootRefusal>() {
            return Some(refusal.into());
        }
        if let Some(refusal) = cause.downcast_ref::<LoadRefusal>() {
            return Some(refusal.into());
        }
        if let Some(violation) = cause.downcast_ref::<CapabilityError>() {
            return Some(violation.into());
        }
        cause
            .downcast_ref::<DigestMismatch>()
            .map(|_| "digest_mismatch")
    })
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
        ServiceKinds::new()
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

/// One entry per manifest, in declaration order; every refusal names the
/// role and the entry path.
async fn load_role<E, L>(
    entries: &[E],
    manifests: Vec<crate::manifest::LoadedManifest>,
    role: Role,
    path: impl Fn(&E) -> &std::path::Path,
    load: impl AsyncFn(&E, crate::manifest::LoadedManifest) -> Result<L>,
) -> Result<Vec<L>> {
    let mut out = Vec::with_capacity(entries.len());
    for (entry, manifest) in entries.iter().zip(manifests) {
        let loaded = load(entry, manifest)
            .await
            .with_context(|| format!("{} {}", role.load_context(), path(entry).display()))?;
        out.push(loaded);
    }
    Ok(out)
}

/// The providers' manifests as the worker install predicates see them.
fn project_manifests(providers: &[LoadedProvider]) -> Vec<ServiceManifest> {
    providers
        .iter()
        .map(|p| ServiceManifest {
            name: p.name.to_string(),
            kind: p.kind,
            sections: p.sections.clone(),
            component_digest: p.seed.artifact.digest,
        })
        .collect()
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
        services = providers.len(),
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

#[cfg(test)]
pub(crate) mod tests;
