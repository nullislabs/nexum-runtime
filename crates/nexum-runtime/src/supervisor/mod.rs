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

pub use load::LoadRefusal;
pub use prepass::{BootRefusal, ConfiguredChains};
pub use store::{WasiClockOverride, build_linker, build_service_linker};
pub use subscriptions::{ChainLogSub, SubscriptionPlan, Viability};

use std::sync::Arc;

use tracing::info;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::engine_config::{EngineConfig, ModuleEntry, ResolvedModuleLimits};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::extension::{Extension, HostServices, ServiceManifest};
use crate::host::state::HostState;
use crate::refusal::{Refusal, RefusalContext as _};
use crate::runtime::poison_policy::PoisonPolicy;
use admission::{ServiceKinds, capability_registry, enforce_extension_uniqueness, service_kinds};
use cursors::ChainLogCursors;
use load::{LoadedModule, LoadedService};
use prepass::{enforce_subscriptions, load_required_manifest, manifest_namespace};
use role::Role;

/// Owns every loaded module and service and exposes the dispatch surface.
pub struct Supervisor<T: RuntimeTypes> {
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    services: Vec<LoadedService>,
    /// Poison-pill thresholds resolved from `[limits.poison]` at boot.
    policy: PoisonPolicy,
    /// In-memory mirror of the persisted chain-log cursors.
    chain_log_cursors: ChainLogCursors,
}

/// Boot inputs derived from [`EngineConfig`], bundled once at the call site.
pub struct BootEnv<'a> {
    /// The engine ceiling a manifest may narrow but never widen.
    pub limits: &'a ResolvedModuleLimits,
    /// Chains with an `engine.toml` entry; a subscription elsewhere refuses.
    pub configured_chains: ConfiguredChains,
    /// Refuse a component whose manifest declares no digest.
    pub require_component_digest: bool,
}

impl<'a> BootEnv<'a> {
    /// Pick the boot-relevant fields out of the loaded config.
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
    /// Admit, compile and initialize every configured component.
    ///
    /// Refusals are counted by kind before they propagate, so a failed
    /// boot is visible in metrics and not only in the log.
    pub async fn boot(
        engine: &Engine,
        linker: &Linker<HostState<T>>,
        engine_cfg: &EngineConfig,
        components: &Components<T>,
        extensions: &[Arc<dyn Extension<T>>],
        clocks: Option<WasiClockOverride>,
    ) -> Result<Self, Refusal> {
        let booted: Result<Self, Refusal> = async {
            let shared = wire_extensions(engine, components, extensions, clocks, true)?;
            let registry = capability_registry(&shared.extensions);
            let prepass = prepass::run(engine_cfg, &registry)?;
            // Services boot first, so every module store built after already
            // routes to the installed instances.
            let services = load_role(
                &engine_cfg.services,
                prepass.adapter_manifests,
                Role::Service,
                |e| &e.path,
                async |entry, manifest| {
                    load::service(
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
            let service_manifests = project_manifests(&services);
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
                        &service_manifests,
                    )
                    .await
                },
            )
            .await?;
            Ok(assemble(
                shared,
                modules,
                services,
                engine_cfg.limits.poison,
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
    ) -> Result<Self, Refusal> {
        let booted: Result<Self, Refusal> = async {
            // Service kinds come only from `engine.toml`, so none register here.
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
                manifest_namespace(&loaded_manifest).as_str(),
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
                services: Vec::new(),
                policy: env.limits.poison,
                chain_log_cursors: ChainLogCursors::default(),
            })
        }
        .await;
        booted.inspect_err(count_boot_refusal)
    }

    /// Modules the supervisor holds, alive or not.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Alive or not.
    pub fn adapter_count(&self) -> usize {
        self.services.len()
    }

    /// Excludes init-failed (permanent) and in-backoff modules.
    pub fn alive_count(&self) -> usize {
        self.modules
            .iter()
            .filter(|m| m.health.dispatchable())
            .count()
    }

    /// Modules quarantined after repeated traps. Only a full engine
    /// restart clears one.
    pub fn poisoned_count(&self) -> usize {
        self.modules
            .iter()
            .filter(|m| m.health.is_poisoned())
            .count()
    }

    /// The per-namespace service map every module store carries.
    pub fn services(&self) -> &HostServices {
        &self.shared.services
    }
}

/// Counts a refusal under its [`Refusal::error_kind`] label; a refusal
/// without a label goes uncounted. The launcher's refusal sites in
/// `builder` call it too, so a launch refusal counts like a boot one.
pub(crate) fn count_boot_refusal(refusal: &Refusal) {
    let Some(kind) = refusal.error_kind() else {
        return;
    };
    metrics::counter!("nexum_runtime_boot_refusals_total", "error_kind" => kind).increment(1);
}

/// The resulting [`Shared`] is the one wiring every later phase reads.
/// `with_service_kinds: false` skips [`service_kinds`], which refuses a serviceless kind.
fn wire_extensions<T: RuntimeTypes>(
    engine: &Engine,
    components: &Components<T>,
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<WasiClockOverride>,
    with_service_kinds: bool,
) -> Result<Shared<T>, Refusal> {
    enforce_extension_uniqueness(extensions)?;
    // A duplicate service namespace is an embedder wiring bug, not an
    // operator refusal, so it stays untyped and uncounted.
    let services = HostServices::from_extensions(extensions).map_err(anyhow::Error::new)?;
    let kinds = if with_service_kinds {
        service_kinds(extensions, &services)?
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
    load: impl AsyncFn(&E, crate::manifest::LoadedManifest) -> Result<L, Refusal>,
) -> Result<Vec<L>, Refusal> {
    let mut out = Vec::with_capacity(entries.len());
    for (entry, manifest) in entries.iter().zip(manifests) {
        let loaded = load(entry, manifest).await.with_refusal_context(|| {
            format!("{} {}", role.load_context(), path(entry).display())
        })?;
        out.push(loaded);
    }
    Ok(out)
}

/// The services' manifests as the worker install predicates see them.
fn project_manifests(services: &[LoadedService]) -> Vec<ServiceManifest> {
    services
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
    services: Vec<LoadedService>,
    policy: PoisonPolicy,
) -> Supervisor<T> {
    let alive = modules.iter().filter(|m| m.health.dispatchable()).count();
    let adapters_alive = services.iter().filter(|s| s.health.dispatchable()).count();
    info!(
        loaded = modules.len(),
        alive,
        services = services.len(),
        adapters_alive,
        "supervisor up"
    );
    Supervisor {
        shared,
        modules,
        services,
        policy,
        chain_log_cursors: ChainLogCursors::default(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
