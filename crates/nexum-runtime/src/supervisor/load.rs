//! Load one module or service: admission, verified compile, instantiation, `init`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Context, Error, Result};
use strum::{IntoStaticStr, VariantNames};
use thiserror::Error as ThisError;
use tracing::{info, warn};
use wasmtime::component::{Component, Linker};

use super::Shared;
use super::admission::{
    ServiceRow, capability_registry, enforce_extension_sections, extension_subscription_vocabulary,
};
use super::artifact::read_verified_component;
use super::dispatch::with_dispatch_deadline;
use super::lifecycle::Health;
use super::prepass::manifest_namespace;
use super::role::Role;
use super::store::{
    HostStore, ResolvedLimits, StoreSpec, build_service_linker, fresh_run_store,
    resolve_module_limits,
};
use crate::bindings::nexum::host::types::Fault;
use crate::bindings::{Config, EventModule};
use crate::digest::ContentDigest;
use crate::engine_config::{ModuleEntry, ResolvedModuleLimits, ServiceEntry};
use crate::host::actor::Liveness;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{Installed, ServiceInstance, ServiceManifest};
use crate::host::logs::RunId;
use crate::host::state::HostState;
use crate::manifest::{self, CapabilityRegistry, ComponentKind, LoadedManifest, Subscription};
use crate::module_id::ModuleId;
use crate::refusal::{Refusal, RefusalContext as _};
use crate::runtime::dispatch_rate::TokenBucket;

/// Admission refusals ahead of instantiation; the wording is operator-pinned.
// `IntoStaticStr`: the snake_case variant name is the `error_kind` label;
// `VariantNames` lets the label-set test enumerate without a value.
#[derive(Debug, ThisError, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum LoadRefusal {
    /// Either a typo in the section key or the claiming extension is not
    /// wired into this composition.
    #[error("{owner} declares manifest section [{section}]; no wired extension claims it")]
    SectionUnclaimed {
        /// The declaring component's namespace.
        owner: String,
        /// The unclaimed section key.
        section: String,
    },
    /// An embedder wiring bug: namespaces key the host service map, so a
    /// duplicate would shadow one extension's service.
    #[error("extension namespace {namespace} is claimed twice")]
    ExtensionNamespaceClaimed {
        /// The doubly claimed namespace.
        namespace: &'static str,
    },
    /// An embedder wiring bug: a subscription kind's events must have one
    /// owning extension.
    #[error("subscription kind {kind} is claimed twice")]
    SubscriptionKindClaimed {
        /// The doubly claimed kind.
        kind: &'static str,
    },
    /// An embedder wiring bug: a section's install predicate must have
    /// one owning extension.
    #[error("manifest section [{section}] is claimed twice")]
    SectionClaimed {
        /// The doubly claimed section key.
        section: &'static str,
    },
    /// An embedder wiring bug: the kind selects the installing extension,
    /// so two registrants are ambiguous.
    #[error("service kind {kind} is registered twice")]
    KindRegisteredTwice {
        /// The doubly registered kind.
        kind: &'static str,
    },
    /// A service kind whose extension owns no host service to install
    /// into.
    #[error("extension {namespace} registers service kind {kind} without a host service")]
    ServicelessKind {
        /// The registering extension's namespace.
        namespace: &'static str,
        /// The serviceless kind.
        kind: &'static str,
    },
    /// The entry belongs under `[[modules]]`, or its manifest kind is
    /// wrong.
    #[error(
        "{} declares the worker kind; an [[services]] entry requires a \
         component.toml declaring a registered service kind ({})",
        path.display(),
        registered.join(", ")
    )]
    WorkerKindAdapter {
        /// The entry's component path.
        path: PathBuf,
        /// The kinds a `[[services]]` entry may declare.
        registered: Vec<&'static str>,
    },
    /// The manifest name selects the kind, so the fix is the name or the
    /// unwired extension.
    #[error(
        "{} declares unregistered service kind {kind}; registered kinds: {}",
        path.display(),
        registered.join(", ")
    )]
    UnregisteredKind {
        /// The entry's component path.
        path: PathBuf,
        /// The kind as the manifest declares it.
        kind: String,
        /// The kinds a `[[services]]` entry may declare.
        registered: Vec<&'static str>,
    },
    /// Either a typo in the subscription kind or its extension is not
    /// wired into this composition.
    #[error(
        "module {module} subscribes to unknown event kind {kind}; no wired extension declares it"
    )]
    UnknownEventKind {
        /// The subscribing module.
        module: ModuleId,
        /// The unknown kind.
        kind: String,
    },
    /// Enforced before compile, so unverified bytes never reach the
    /// compiler.
    #[error(
        "no [component].digest digest for {} and [engine] require_component_digest is set; \
         pin the artifact's sha256 in its component.toml",
        path.display()
    )]
    DigestUnpinned {
        /// The unpinned entry's component path.
        path: PathBuf,
    },
}

/// Restarts reuse the cache, so the boot-time digest holds for every run.
pub(super) struct CachedArtifact {
    /// `Component` is internally `Arc`-backed, so the cache is cheap.
    pub(super) component: Component,
    /// sha256 of the loaded artifact bytes, computed even when unpinned.
    pub(super) digest: ContentDigest,
    pub(super) init_config: Config,
}

/// Everything needed to rebuild a store and re-run `init` or reinstall.
pub(super) struct Seed {
    pub(super) artifact: CachedArtifact,
    pub(super) spec: StoreSpec,
    /// Wall-clock bound on a whole dispatch, host calls included.
    pub(super) event_deadline: Duration,
}

impl Seed {
    /// The borrow of the cached component ends when `install` returns.
    pub(super) fn instance<'a, T: RuntimeTypes>(
        &'a self,
        linker: &'a Linker<HostState<T>>,
        sections: &'a manifest::ExtensionSections,
        store: HostStore<T>,
        liveness: Liveness,
    ) -> ServiceInstance<'a, T> {
        ServiceInstance {
            component: &self.artifact.component,
            linker,
            store,
            config: self.artifact.init_config.clone(),
            sections,
            fuel_per_call: self.spec.fuel,
            liveness,
        }
    }
}

/// Restarts replace bindings, store, and run; the rate bucket carries across.
pub(super) struct LiveInstance<T: RuntimeTypes> {
    pub(super) bindings: EventModule,
    pub(super) store: HostStore<T>,
    pub(super) run: RunId,
    pub(super) dispatch_bucket: TokenBucket,
}

pub(super) struct LoadedModule<T: RuntimeTypes> {
    pub(super) name: ModuleId,
    pub(super) live: LiveInstance<T>,
    pub(super) seed: Seed,
    pub(super) subscriptions: Vec<Subscription>,
    pub(super) health: Health,
}

pub(super) struct LoadedService {
    /// The service's namespace: its manifest name.
    pub(super) name: ModuleId,
    pub(super) kind: &'static str,
    pub(super) sections: manifest::ExtensionSections,
    pub(super) seed: Seed,
    /// Trap signal shared with the installed actor; feeds `health` at
    /// sweep time and carries no lifecycle authority of its own.
    pub(super) liveness: Liveness,
    pub(super) run: RunId,
    /// Alive against a dead `liveness` is an unrecorded trap the next sweep records.
    pub(super) health: Health,
}

/// Every refusal precedes compile cost.
fn admit_and_verify<T: RuntimeTypes, R>(
    shared: &Shared<T>,
    owner: &str,
    path: &Path,
    loaded_manifest: &LoadedManifest,
    registry: &CapabilityRegistry,
    require_component_digest: bool,
    admit: impl FnOnce() -> Result<R, Refusal>,
) -> Result<(R, Component, ContentDigest), Refusal> {
    enforce_extension_sections(owner, &loaded_manifest.extensions, &shared.extensions)?;
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
    .with_refusal_context(|| format!("capability violation in {}", path.display()))?;
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

/// Runs under the dispatch deadline so a hung host call cannot park boot or a
/// restart; a deadline hit or trap is `Err`, a guest fault `Ok(Err(fault))`.
async fn run_init<T: RuntimeTypes>(
    bindings: &EventModule,
    store: &mut HostStore<T>,
    config: &Config,
    deadline: Duration,
) -> Result<Result<(), Fault>> {
    Ok(with_dispatch_deadline(deadline, bindings.call_init(store, config)).await??)
}

/// Instantiates the cached component on a fresh store and runs `init`; what
/// a guest init fault means (dead at boot, deferred on restart) stays with
/// the caller.
pub(super) async fn instantiate_module<T: RuntimeTypes>(
    linker: &Linker<HostState<T>>,
    seed: &Seed,
    name: &ModuleId,
    store: &mut HostStore<T>,
) -> Result<(EventModule, Result<(), Fault>)> {
    let bindings = EventModule::instantiate_async(&mut *store, &seed.artifact.component, linker)
        .await
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("instantiate {name}"))?;
    let init = run_init(
        &bindings,
        store,
        &seed.artifact.init_config,
        seed.event_deadline,
    )
    .await?;
    Ok((bindings, init))
}

/// Builds the kind's linker and installs on the given store; a `Dead`
/// verdict carries no error, its meaning stays with the caller.
/// `event_deadline` bounds the whole install (instantiation, guest `init`,
/// extension wiring), not only the guest call a module's bound covers.
pub(super) async fn install_service<T: RuntimeTypes>(
    shared: &Shared<T>,
    row: &ServiceRow<T>,
    seed: &Seed,
    sections: &manifest::ExtensionSections,
    store: HostStore<T>,
    liveness: Liveness,
) -> Result<Installed> {
    let (kind, service) = row;
    let linker = build_service_linker::<T>(&shared.engine, kind.as_ref())?;
    with_dispatch_deadline(
        seed.event_deadline,
        kind.install(seed.instance(&linker, sections, store, liveness), service),
    )
    .await
    .with_context(|| format!("service kind {} did not install in time", kind.kind()))?
}

/// A failed `init` loads the module dead; the dispatcher skips it.
pub(super) async fn module<T: RuntimeTypes>(
    shared: &Shared<T>,
    linker: &Linker<HostState<T>>,
    entry: &ModuleEntry,
    loaded_manifest: LoadedManifest,
    limits_cfg: &ResolvedModuleLimits,
    require_component_digest: bool,
    service_manifests: &[ServiceManifest],
) -> Result<LoadedModule<T>, Refusal> {
    let module_namespace: ModuleId = manifest_namespace(&loaded_manifest);
    let registry = capability_registry(&shared.extensions);
    let sections = &loaded_manifest.extensions;
    let ((), component, digest) = admit_and_verify(
        shared,
        module_namespace.as_str(),
        &entry.path,
        &loaded_manifest,
        &registry,
        require_component_digest,
        || {
            for ext in &shared.extensions {
                ext.admit_worker(module_namespace.as_str(), sections, service_manifests)
                    .with_refusal_context(|| {
                        format!("install refused for {}", entry.path.display())
                    })?;
            }
            info!(component = %entry.path.display(), "compiling component");
            Ok(())
        },
    )?;

    let ResolvedLimits {
        fuel,
        memory,
        state_bytes,
    } = resolve_module_limits(&loaded_manifest.resources, limits_cfg);
    info!(
        module = %module_namespace,
        fuel,
        memory_bytes = memory,
        state_bytes,
        "applied module resource limits",
    );
    let spec = StoreSpec {
        http_allowlist: loaded_manifest.http_allowlist.clone(),
        http_limits: limits_cfg.http,
        http_permitted: limits_cfg.http_permit_destinations.clone(),
        memory_limit: memory,
        fuel,
        chain_response_max_bytes: limits_cfg.chain_response_max_bytes.get(),
        state_quota: state_bytes,
    };
    let config = default_init_config(&loaded_manifest.config, module_namespace.as_str());
    let seed = Seed {
        artifact: CachedArtifact {
            component,
            digest,
            init_config: config,
        },
        spec,
        event_deadline: limits_cfg.event_deadline,
    };
    let (run, mut store) = fresh_run_store(shared, &module_namespace, 0, &seed.spec, Role::Module)?;
    let (bindings, init) = instantiate_module(linker, &seed, &module_namespace, &mut store).await?;
    // A failed `init` leaves guest state uninitialized, so the module loads dead.
    let init_succeeded = match init {
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
    // Unserviceable subscriptions warn; an undeclared extension kind refuses.
    let extension_kinds = extension_subscription_vocabulary(&shared.extensions);
    for sub in &loaded_manifest.subscriptions {
        match sub {
            Subscription::Cron { .. } => warn!(
                module = %module_namespace,
                "cron subscriptions are declared but inert in 0.2 (lands in 0.3)",
            ),
            Subscription::Extension { kind, .. } if !extension_kinds.contains(kind.as_str()) => {
                return Err(LoadRefusal::UnknownEventKind {
                    module: module_namespace.clone(),
                    kind: kind.clone(),
                }
                .into());
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
            dispatch_bucket: TokenBucket::new(limits_cfg.dispatch, Instant::now()),
        },
        seed,
        subscriptions: loaded_manifest.subscriptions.clone(),
        health: Health::from_init(init_succeeded),
    })
}

/// A failed `init` loads the service dead and unroutable, permanently.
pub(super) async fn service<T: RuntimeTypes>(
    shared: &Shared<T>,
    entry: &ServiceEntry,
    loaded_manifest: LoadedManifest,
    limits_cfg: &ResolvedModuleLimits,
    require_component_digest: bool,
) -> Result<LoadedService, Refusal> {
    let namespace: ModuleId = manifest_namespace(&loaded_manifest);
    // A core-only declaration fails at manifest load; an undeclared gated
    // import fails after compile; the linker withholds the core interfaces.
    let registry = CapabilityRegistry::service();
    let sections = loaded_manifest.extensions.clone();
    let (row, component, digest) = admit_and_verify(
        shared,
        namespace.as_str(),
        &entry.path,
        &loaded_manifest,
        &registry,
        require_component_digest,
        || {
            for ext in &shared.extensions {
                ext.admit_service(namespace.as_str(), &sections)
                    .with_refusal_context(|| {
                        format!("install refused for {}", entry.path.display())
                    })?;
            }
            // An unregistered kind refuses before compile.
            // A service's name is the service type, so the name selects
            // the row. A module declared as an adapter refuses before compile.
            let row: &ServiceRow<T> = match loaded_manifest.kind {
                ComponentKind::Module => {
                    return Err(LoadRefusal::WorkerKindAdapter {
                        path: entry.path.clone(),
                        registered: super::admission::registered_kinds(&shared.kinds),
                    }
                    .into());
                }
                ComponentKind::Service => {
                    let name = loaded_manifest.name.as_str();
                    shared
                        .kinds
                        .get(name)
                        .ok_or_else(|| LoadRefusal::UnregisteredKind {
                            path: entry.path.clone(),
                            kind: name.to_owned(),
                            registered: super::admission::registered_kinds(&shared.kinds),
                        })?
                }
            };
            info!(
                component = %entry.path.display(),
                kind = row.0.kind(),
                "compiling service component",
            );
            Ok(row)
        },
    )?;
    let kind = row.0.as_ref();

    info!(
        service = %namespace,
        kind = kind.kind(),
        fuel = limits_cfg.fuel_per_event.get(),
        memory_bytes = limits_cfg.memory_bytes.get(),
        http_allow = entry.http_allow.len(),
        "applied service resource limits and transport scope",
    );

    let spec = StoreSpec {
        http_allowlist: entry.http_allow.clone(),
        http_limits: limits_cfg.http,
        http_permitted: limits_cfg.http_permit_destinations.clone(),
        memory_limit: limits_cfg.memory_bytes.get(),
        fuel: limits_cfg.fuel_per_event.get(),
        chain_response_max_bytes: limits_cfg.chain_response_max_bytes.get(),
        state_quota: limits_cfg.state_bytes,
    };
    let config = default_init_config(&loaded_manifest.config, namespace.as_str());
    let seed = Seed {
        artifact: CachedArtifact {
            component,
            digest,
            init_config: config,
        },
        spec,
        event_deadline: limits_cfg.event_deadline,
    };
    let liveness = Liveness::default();
    let (run, store) = fresh_run_store(shared, &namespace, 0, &seed.spec, Role::Service)?;
    let installed = install_service(shared, row, &seed, &sections, store, liveness.clone())
        .await
        .with_refusal_context(|| format!("install {}", entry.path.display()))?;
    // A dead install at boot is permanent; the liveness records it for the sweep.
    if installed == Installed::Dead {
        liveness.mark_dead();
    }
    Ok(LoadedService {
        name: namespace,
        kind: kind.kind(),
        sections,
        seed,
        liveness,
        run,
        health: Health::from_init(installed == Installed::Live),
    })
}
