//! Multi-module supervisor: loads `engine.toml` entries, one wasmtime `Store`
//! each, and routes triggers.

mod admission;
mod artifact;
mod cursors;
mod dispatch;
mod lifecycle;
pub(crate) mod load;
pub(crate) mod prepass;
mod store;
mod triggers;

pub use load::LoadRefusal;
pub use prepass::{BootRefusal, ConfiguredChains};
pub use store::{WasiClockOverride, build_linker};
pub use triggers::{EventTrigger, TriggerPlan, Viability};

use std::sync::Arc;

use nexum_tasks::Shutdown;
use tracing::info;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::engine_config::{EngineConfig, ModuleEntry, PolicySection, ResolvedModuleLimits};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::extension::Extension;
use crate::host::state::HostState;
use crate::refusal::{Refusal, RefusalContext as _};
use crate::runtime::poison_policy::PoisonPolicy;
use admission::{capability_registry, enforce_extension_uniqueness};
use cursors::ChainLogCursors;
use load::LoadedModule;
use prepass::{enforce_triggers, load_required_manifest, manifest_namespace};

/// Owns every loaded module.
pub struct Supervisor<T: RuntimeTypes> {
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    /// Poison-pill thresholds resolved from `[limits.poison]` at boot.
    policy: PoisonPolicy,
    /// In-memory mirror of the persisted chain-log cursors.
    chain_log_cursors: ChainLogCursors,
    /// Once fired, the dispatch fan-out halts between guest calls, so the
    /// shutdown drain covers at most one in-flight call.
    stop: Option<Shutdown>,
}

/// Boot inputs derived from [`EngineConfig`], bundled once at the call site.
pub struct BootEnv<'a> {
    /// Per-module limits outside the `[policy]` ceilings.
    pub limits: &'a ResolvedModuleLimits,
    /// The `[policy]` surface a manifest may narrow but never widen.
    pub policy: &'a PolicySection,
    /// Chains with an `engine.toml` entry; a trigger elsewhere refuses.
    pub configured_chains: ConfiguredChains,
    /// Refuse a component whose manifest declares no digest.
    pub require_component_digest: bool,
}

impl<'a> BootEnv<'a> {
    /// Pick the boot-relevant fields out of the loaded config.
    pub fn from_config(cfg: &'a EngineConfig) -> Self {
        Self {
            limits: &cfg.limits,
            policy: &cfg.policy,
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
            let shared = wire_extensions(engine, components, extensions, clocks)?;
            let registry = capability_registry(&shared.extensions);
            let module_manifests = prepass::run(engine_cfg, &registry)?;
            let env = BootEnv::from_config(engine_cfg);
            let modules = load_modules(
                &engine_cfg.modules,
                module_manifests,
                async |entry, manifest, resolved| {
                    load::module(&shared, linker, entry, manifest, resolved, &env).await
                },
            )
            .await?;
            Ok(assemble(shared, modules, engine_cfg.limits.poison))
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
            let shared = wire_extensions(engine, components, extensions, clocks)?;
            let registry = capability_registry(&shared.extensions);
            let loaded_manifest =
                load_required_manifest(&entry.path, entry.manifest.as_deref(), &registry)?;
            enforce_triggers(
                manifest_namespace(&loaded_manifest).as_str(),
                &loaded_manifest,
                &env.configured_chains,
            )?;
            let resolved = store::resolve_module_limits(
                &entry.id,
                &loaded_manifest.resources,
                &env.policy.for_component(&entry.id).ceilings,
            );
            prepass::enforce_total_reservation(env.policy, [(entry.id.as_str(), resolved.memory)])?;
            let loaded =
                load::module(&shared, linker, entry, loaded_manifest, resolved, env).await?;
            Ok(Self {
                shared,
                modules: vec![loaded],
                policy: env.limits.poison,
                chain_log_cursors: ChainLogCursors::default(),
                stop: None,
            })
        }
        .await;
        booted.inspect_err(count_boot_refusal)
    }

    /// Halt the dispatch fan-out between guest calls once `stop` fires; a
    /// skipped event replays through its resume cursor, a skipped
    /// block does not.
    pub fn stop_on(&mut self, stop: Shutdown) {
        self.stop = Some(stop);
    }

    fn stop_requested(&self) -> bool {
        self.stop.as_ref().is_some_and(Shutdown::is_fired)
    }

    /// Modules the supervisor holds, alive or not.
    pub fn module_count(&self) -> usize {
        self.modules.len()
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
fn wire_extensions<T: RuntimeTypes>(
    engine: &Engine,
    components: &Components<T>,
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<WasiClockOverride>,
) -> Result<Shared<T>, Refusal> {
    enforce_extension_uniqueness(extensions)?;
    Ok(Shared {
        engine: engine.clone(),
        components: components.clone(),
        extensions: extensions.to_vec(),
        clocks,
    })
}

/// One entry per manifest, in declaration order; every refusal names the
/// entry path.
async fn load_modules<L>(
    entries: &[ModuleEntry],
    manifests: Vec<(crate::manifest::LoadedManifest, store::ResolvedLimits)>,
    load: impl AsyncFn(
        &ModuleEntry,
        crate::manifest::LoadedManifest,
        store::ResolvedLimits,
    ) -> Result<L, Refusal>,
) -> Result<Vec<L>, Refusal> {
    let mut out = Vec::with_capacity(entries.len());
    for (entry, (manifest, resolved)) in entries.iter().zip(manifests) {
        let loaded = load(entry, manifest, resolved)
            .await
            .with_refusal_context(|| format!("load module {}", entry.path.display()))?;
        out.push(loaded);
    }
    Ok(out)
}

fn assemble<T: RuntimeTypes>(
    shared: Shared<T>,
    modules: Vec<LoadedModule<T>>,
    policy: PoisonPolicy,
) -> Supervisor<T> {
    let alive = modules.iter().filter(|m| m.health.dispatchable()).count();
    info!(loaded = modules.len(), alive, "supervisor up");
    Supervisor {
        shared,
        modules,
        policy,
        chain_log_cursors: ChainLogCursors::default(),
        stop: None,
    }
}

#[cfg(test)]
pub(crate) mod tests;
