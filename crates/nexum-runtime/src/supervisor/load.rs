//! Load one module or provider: admission, verified compile,
//! instantiation, and `init`. A module retains its store and bindings; a
//! provider's store is consumed by `kind.install`, so the two loaders stay
//! role-specific.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, anyhow};
use tracing::{info, warn};
use wasmtime::component::{Component, Linker};

use super::Shared;
use super::admission::{
    ProviderRow, capability_registry, enforce_extension_sections, extension_subscription_vocabulary,
};
use super::artifact::read_verified_component;
use super::dispatch::with_dispatch_deadline;
use super::lifecycle::Health;
use super::prepass::{MODULE_FALLBACK_NAME, PROVIDER_FALLBACK_NAME, manifest_namespace};
use super::store::{
    self, HostStore, ResolvedLimits, StoreSpec, build_provider_linker, resolve_module_limits,
};
use crate::bindings::nexum::host::types::Fault;
use crate::bindings::{Config, EventModule};
use crate::digest::ContentDigest;
use crate::engine_config::{AdapterEntry, ModuleEntry, ModuleLimits};
use crate::host::actor::Liveness;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{HostServices, Installed, ProviderInstance, ProviderManifest};
use crate::host::logs::RunId;
use crate::host::state::HostState;
use crate::manifest::{self, CapabilityRegistry, ComponentKind, LoadedManifest, Subscription};
use crate::module_id::ModuleId;
use crate::runtime::dispatch_rate::TokenBucket;

/// The compiled artifact and `init` inputs cached for restarts; restarts
/// reuse these, so the boot-time digest holds for every run.
pub(super) struct CachedArtifact {
    /// `Component` is internally `Arc`-backed, so the cache is cheap.
    pub(super) component: Component,
    /// sha256 of the loaded artifact bytes, computed even when unpinned.
    pub(super) digest: ContentDigest,
    /// The manifest `[config]` passed to `init`.
    pub(super) init_config: Config,
}

/// Everything needed to rebuild a module's store and re-run `init`.
pub(super) struct ModuleSeed {
    pub(super) artifact: CachedArtifact,
    pub(super) spec: StoreSpec,
    /// Wall-clock deadline for a whole dispatch (guest plus every host
    /// call); the backstop for a dispatch parked in a host call.
    pub(super) event_deadline: Duration,
}

/// Everything needed to rebuild a provider's store and reinstall it.
pub(super) struct ProviderSeed {
    pub(super) artifact: CachedArtifact,
    pub(super) spec: StoreSpec,
}

/// The live half of a loaded module: the instantiated run a dispatch
/// enters. Restarts replace bindings, store, and run; the rate bucket
/// carries across restarts.
pub(super) struct LiveInstance<T: RuntimeTypes> {
    pub(super) bindings: EventModule,
    pub(super) store: HostStore<T>,
    /// The run this store instantiates; restarts mint a fresh `RunId` with
    /// an incremented sequence.
    pub(super) run: RunId,
    /// Per-module dispatch rate limiter, checked before the guest runs;
    /// over-rate events are dropped and counted.
    pub(super) dispatch_bucket: TokenBucket,
}

pub(super) struct LoadedModule<T: RuntimeTypes> {
    pub(super) name: ModuleId,
    pub(super) live: LiveInstance<T>,
    pub(super) seed: ModuleSeed,
    /// Subscriptions copied from `module.toml`, read on every event to
    /// decide dispatch.
    pub(super) subscriptions: Vec<Subscription>,
    /// Lifecycle authority: alive/backoff/dead/poisoned plus the failure
    /// history. Traps are recorded eagerly by the dispatch arm.
    pub(super) health: Health,
}

/// One loaded provider; mirrors [`LoadedModule`]'s restart and poison
/// bookkeeping. Liveness is shared with the installed actor.
pub(super) struct LoadedProvider {
    /// The provider's namespace: its manifest name.
    pub(super) name: ModuleId,
    /// Registered kind the restart sweep reinstalls through.
    pub(super) kind: &'static str,
    /// Extension-owned manifest sections.
    pub(super) sections: manifest::ExtensionSections,
    pub(super) seed: ProviderSeed,
    /// Trap signal shared with the installed actor; feeds `health` at
    /// sweep time and carries no lifecycle authority of its own.
    pub(super) liveness: Liveness,
    /// The run currently installed; a revive mints the successor and
    /// commits it only on a live install.
    pub(super) run: RunId,
    /// Lifecycle authority: `health` alive against a dead `liveness` is an
    /// unrecorded trap the next sweep records.
    pub(super) health: Health,
}

/// Shared admission prologue: refuse an unclaimed manifest section, run the
/// role-specific `admit` step, then verify, compile, and capability-check
/// the artifact. Any refusal precedes compile cost.
fn admit_and_verify<T: RuntimeTypes, R>(
    shared: &Shared<T>,
    owner: &str,
    path: &Path,
    loaded_manifest: &LoadedManifest,
    registry: &CapabilityRegistry,
    require_component_digest: bool,
    admit: impl FnOnce() -> Result<R>,
) -> Result<(R, Component, ContentDigest)> {
    enforce_extension_sections(
        owner,
        &loaded_manifest.manifest.extensions,
        &shared.extensions,
    )?;
    let admitted = admit()?;
    let (component, digest) = read_verified_component(
        &shared.engine,
        path,
        loaded_manifest.component_digest.as_ref(),
        require_component_digest,
    )?;
    manifest::enforce_capabilities(
        loaded_manifest,
        component
            .component_type()
            .imports(&shared.engine)
            .map(|(n, _)| n),
        registry,
    )
    .with_context(|| format!("capability violation in {}", path.display()))?;
    Ok((admitted, component, digest))
}

/// The manifest `[config]`, or the `name` fallback when it declares none.
fn default_init_config(config: &Config, namespace: &str) -> Config {
    if config.is_empty() {
        vec![("name".into(), namespace.to_owned())]
    } else {
        config.clone()
    }
}

/// Call `init` under the dispatch wall-clock deadline, so a hung host call
/// during init cannot park boot or a restart. A deadline hit or trap is
/// `Err`; a guest-returned fault is `Ok(Err(fault))` so each call site
/// applies its own policy (boot loads dead-permanent, restart defers).
pub(super) async fn run_init<T: RuntimeTypes>(
    bindings: &EventModule,
    store: &mut HostStore<T>,
    config: &Config,
    deadline: Duration,
) -> Result<Result<(), Fault>> {
    with_dispatch_deadline(deadline, bindings.call_init(store, config))
        .await
        .map_err(Error::from)?
        .map_err(Error::from)
}

/// Load one `[[modules]]` entry; a failed `init` loads the module dead so
/// the dispatcher skips it.
pub(super) async fn module<T: RuntimeTypes>(
    shared: &Shared<T>,
    linker: &Linker<HostState<T>>,
    entry: &ModuleEntry,
    loaded_manifest: LoadedManifest,
    limits_cfg: &ModuleLimits,
    require_component_digest: bool,
    provider_manifests: &[ProviderManifest],
) -> Result<LoadedModule<T>> {
    let module_namespace: ModuleId =
        manifest_namespace(&loaded_manifest, MODULE_FALLBACK_NAME).into();
    let registry = capability_registry(&shared.extensions);
    let sections = &loaded_manifest.manifest.extensions;
    let ((), component, digest) = admit_and_verify(
        shared,
        module_namespace.as_str(),
        &entry.path,
        &loaded_manifest,
        &registry,
        require_component_digest,
        || {
            for ext in &shared.extensions {
                ext.admit_worker(module_namespace.as_str(), sections, provider_manifests)
                    .with_context(|| format!("install refused for {}", entry.path.display()))?;
            }
            info!(component = %entry.path.display(), "compiling component");
            Ok(())
        },
    )?;

    let ResolvedLimits {
        fuel,
        memory,
        state_bytes,
    } = resolve_module_limits(&loaded_manifest.manifest.module.resources, limits_cfg);
    info!(
        module = %module_namespace,
        fuel,
        memory_bytes = memory,
        state_bytes,
        "applied module resource limits",
    );
    let spec = StoreSpec {
        http_allowlist: loaded_manifest.http_allowlist.clone(),
        http_limits: limits_cfg.http(),
        // Event modules are unscoped for messaging; only providers carry
        // a topic grant.
        messaging_topics: Vec::new(),
        memory_limit: memory,
        fuel,
        chain_response_max_bytes: limits_cfg.chain_response_max_bytes(),
        state_quota: state_bytes,
    };
    // First run of this module: sequence 0. Restarts increment it.
    let run = RunId::new(module_namespace.clone(), 0);
    let mut store = store::build(shared, &spec, run.clone(), shared.services.clone())?;
    let bindings = EventModule::instantiate_async(&mut store, &component, linker)
        .await
        .map_err(Error::from)
        .with_context(|| format!("instantiate {}", entry.path.display()))?;

    let config = default_init_config(&loaded_manifest.config, module_namespace.as_str());
    // A failed `init` leaves guest state uninitialised, so the module
    // loads dead and the dispatcher skips it rather than waste dispatches.
    let init_succeeded =
        match run_init(&bindings, &mut store, &config, limits_cfg.event_deadline()).await? {
            Ok(()) => {
                info!(module = %module_namespace, "init succeeded");
                true
            }
            Err(e) => {
                warn!(
                    module = %module_namespace,
                    kind = crate::host::error::fault_label(&e),
                    message = %crate::host::error::fault_message(&e),
                    "init failed - module loaded but marked dead; dispatcher will skip it",
                );
                false
            }
        };
    // Refuel after init so the first on_event starts with a full budget.
    store.set_fuel(fuel)?;

    // Surface any `[[subscription]]` entries the host cannot service yet,
    // and refuse an extension kind no wired extension declares.
    let extension_kinds = extension_subscription_vocabulary(&shared.extensions);
    for sub in &loaded_manifest.manifest.subscriptions {
        match sub {
            Subscription::Cron { .. } => warn!(
                module = %module_namespace,
                "cron subscriptions are declared but inert in 0.2 (lands in 0.3)",
            ),
            Subscription::Extension { kind, .. } if !extension_kinds.contains(kind.as_str()) => {
                return Err(anyhow!(
                    "module {module_namespace} subscribes to unknown event kind {kind}; \
                     no wired extension declares it"
                ));
            }
            _ => {}
        }
    }

    Ok(LoadedModule {
        name: module_namespace,
        live: LiveInstance {
            bindings,
            store,
            run,
            dispatch_bucket: TokenBucket::new(limits_cfg.dispatch_rate(), Instant::now()),
        },
        seed: ModuleSeed {
            artifact: CachedArtifact {
                component,
                digest,
                init_config: config,
            },
            spec,
            event_deadline: limits_cfg.event_deadline(),
        },
        subscriptions: loaded_manifest.manifest.subscriptions.clone(),
        health: if init_succeeded {
            Health::alive()
        } else {
            Health::dead()
        },
    })
}

/// Load one `[[adapters]]` entry; a failed `init` loads the provider dead
/// and unroutable, permanently.
pub(super) async fn provider<T: RuntimeTypes>(
    shared: &Shared<T>,
    entry: &AdapterEntry,
    loaded_manifest: LoadedManifest,
    limits_cfg: &ModuleLimits,
    require_component_digest: bool,
) -> Result<LoadedProvider> {
    let namespace: ModuleId = manifest_namespace(&loaded_manifest, PROVIDER_FALLBACK_NAME).into();
    // The provider registry scopes capabilities to transports: a core-only
    // declaration fails at manifest load, an undeclared transport import
    // fails after compile, and the linker withholds the same interfaces.
    let registry = CapabilityRegistry::provider();
    let sections = loaded_manifest.manifest.extensions.clone();
    let ((kind, service), component, digest) = admit_and_verify(
        shared,
        namespace.as_str(),
        &entry.path,
        &loaded_manifest,
        &registry,
        require_component_digest,
        || {
            for ext in &shared.extensions {
                ext.admit_provider(namespace.as_str(), &sections)
                    .with_context(|| format!("install refused for {}", entry.path.display()))?;
            }
            // The manifest kind is the discriminator: an [[adapters]] entry
            // must name a registered provider kind, caught before compile.
            let (kind, service): &ProviderRow<T> = match &loaded_manifest.manifest.module.kind {
                ComponentKind::Worker => {
                    return Err(anyhow!(
                        "{} declares the worker kind; an [[adapters]] entry requires a \
                         module.toml declaring a registered provider kind ({})",
                        entry.path.display(),
                        super::admission::registered_kinds(&shared.kinds),
                    ));
                }
                ComponentKind::Provider(spelling) => {
                    shared.kinds.get(spelling.as_str()).ok_or_else(|| {
                        anyhow!(
                            "{} declares unregistered provider kind {spelling}; registered \
                                 kinds: {}",
                            entry.path.display(),
                            super::admission::registered_kinds(&shared.kinds),
                        )
                    })?
                }
            };
            info!(
                component = %entry.path.display(),
                kind = kind.kind(),
                "compiling provider component",
            );
            Ok((kind, service))
        },
    )?;

    info!(
        provider = %namespace,
        kind = kind.kind(),
        fuel = limits_cfg.fuel(),
        memory_bytes = limits_cfg.memory(),
        http_allow = entry.http_allow.len(),
        messaging_topics = entry.messaging_topics.len(),
        "applied provider resource limits and transport scope",
    );

    let linker = build_provider_linker::<T>(&shared.engine, kind.as_ref())?;
    let spec = StoreSpec {
        http_allowlist: entry.http_allow.clone(),
        http_limits: limits_cfg.http(),
        messaging_topics: entry.messaging_topics.clone(),
        memory_limit: limits_cfg.memory(),
        fuel: limits_cfg.fuel(),
        chain_response_max_bytes: limits_cfg.chain_response_max_bytes(),
        state_quota: limits_cfg.state_bytes(),
    };
    let run = RunId::new(namespace.clone(), 0);
    // A provider links no service-consuming import, so its store carries
    // an empty service map; the shared map holds the registry that owns
    // the provider's store, and carrying it here would cycle.
    let store = store::build(shared, &spec, run.clone(), HostServices::default())?;

    let config = default_init_config(&loaded_manifest.config, namespace.as_str());
    let liveness = Liveness::default();
    let installed = kind
        .install(
            ProviderInstance {
                component: &component,
                linker: &linker,
                store,
                config: config.clone(),
                sections: &sections,
                fuel_per_call: limits_cfg.fuel(),
                liveness: liveness.clone(),
            },
            service,
        )
        .await
        .with_context(|| format!("install {}", entry.path.display()))?;
    if installed == Installed::Dead {
        liveness.mark_dead();
    }
    Ok(LoadedProvider {
        name: namespace,
        kind: kind.kind(),
        sections,
        seed: ProviderSeed {
            artifact: CachedArtifact {
                component,
                digest,
                init_config: config,
            },
            spec,
        },
        liveness,
        run,
        health: if installed == Installed::Live {
            Health::alive()
        } else {
            Health::dead()
        },
    })
}
