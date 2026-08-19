//! Load one module: admission, verified compile, instantiation, `init`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Context, Error, Result};
use strum::{IntoStaticStr, VariantNames};
use thiserror::Error as ThisError;
use tracing::{info, warn};
use wasmtime::component::{Component, Linker};

use super::admission::{capability_registry, enforce_extension_sections, extension_trigger_kinds};
use super::artifact::{DigestPolicy, read_verified_component};
use super::dispatch::with_dispatch_deadline;
use super::lifecycle::Health;
use super::prepass::manifest_namespace;
use super::store::{HostStore, ResolvedLimits, StoreSpec, fresh_run_store};
use super::{BootEnv, Shared};
use crate::bindings::nexum::host::types::Fault;
use crate::bindings::{Config, TriggerModule};
use crate::engine_config::ModuleEntry;
use crate::error::{EngineRefusal, RefusalContext as _, RuntimeError};
use crate::manifest::{self, CapabilityRegistry, LoadedManifest, Trigger};
use crate::runtime::dispatch_rate::TokenBucket;
use nexum_primitives::digest::ContentDigest;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_logs::RunId;
use nexum_runtime_wasm::HostState;

/// Admission refusals ahead of instantiation; the wording is operator-pinned.
// `IntoStaticStr`: the snake_case variant name is the `error_kind` label;
// `VariantNames` lets the label-set test enumerate without a value.
#[derive(Debug, ThisError, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
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
    /// An embedder wiring bug: an extension's namespace must have one
    /// owner.
    #[error("extension namespace {namespace} is claimed twice")]
    ExtensionNamespaceClaimed {
        /// The doubly claimed namespace.
        namespace: &'static str,
    },
    /// An embedder wiring bug: a trigger kind must have one owning
    /// extension.
    #[error("trigger kind {kind} is claimed twice")]
    TriggerKindClaimed {
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
    /// Either a typo in the trigger kind or its extension is not wired
    /// into this composition.
    #[error("module {module} declares unknown trigger kind {kind}; no wired extension declares it")]
    UnknownTriggerKind {
        /// The declaring module.
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
    /// The operator's capability allowlist is the ceiling; a manifest
    /// declaration cannot widen it (ADR-0001: grant whole or refuse).
    #[error(
        "component {id} declares capability {capability}; \
         [policy].capabilities permits only: {permitted}"
    )]
    CapabilityNotPermitted {
        /// The entry's operator-written id.
        id: String,
        /// The declared capability the policy excludes.
        capability: String,
        /// The permitted set, for the fix.
        permitted: String,
    },
    /// Chain data reaches the guest through `on_trigger`, not an import,
    /// so the trigger is gated on the same operator grant as the `chain`
    /// dependency.
    #[error(
        "component {id} declares a chain trigger; \
         [policy].capabilities permits only: {permitted}"
    )]
    ChainTriggerNotPermitted {
        /// The entry's operator-written id.
        id: String,
        /// The permitted set, for the fix.
        permitted: String,
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

/// Everything needed to rebuild a store and re-run `init`.
pub(super) struct Seed {
    pub(super) artifact: CachedArtifact,
    pub(super) spec: StoreSpec,
    /// Wall-clock bound on a whole dispatch, host calls included.
    pub(super) dispatch_deadline: Duration,
}

/// Restarts replace bindings, store, and run; the rate bucket carries across.
pub(super) struct LiveInstance<T: RuntimeTypes> {
    pub(super) bindings: TriggerModule,
    pub(super) store: HostStore<T>,
    pub(super) run: RunId,
    pub(super) dispatch_bucket: TokenBucket,
}

pub(super) struct LoadedModule<T: RuntimeTypes> {
    pub(super) name: ModuleId,
    pub(super) live: LiveInstance<T>,
    pub(super) seed: Seed,
    pub(super) triggers: Vec<Trigger>,
    pub(super) health: Health,
}

/// Every refusal precedes compile cost.
fn admit_and_verify<T: RuntimeTypes, R>(
    shared: &Shared<T>,
    owner: &str,
    path: &Path,
    loaded_manifest: &LoadedManifest,
    registry: &CapabilityRegistry,
    pins: DigestPolicy<'_>,
    admit: impl FnOnce() -> Result<R, RuntimeError>,
) -> Result<(R, Component, ContentDigest), RuntimeError> {
    enforce_extension_sections(owner, &loaded_manifest.extensions, &shared.extensions)?;
    let admitted = admit()?;
    let (component, digest) = read_verified_component(&shared.engine, path, pins)?;
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
    bindings: &TriggerModule,
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
) -> Result<(TriggerModule, Result<(), Fault>)> {
    let bindings = TriggerModule::instantiate_async(&mut *store, &seed.artifact.component, linker)
        .await
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("instantiate {name}"))?;
    let init = run_init(
        &bindings,
        store,
        &seed.artifact.init_config,
        seed.dispatch_deadline,
    )
    .await?;
    Ok((bindings, init))
}

/// `[policy].capabilities` bounds what a manifest may declare, so the
/// component's imports (already checked against the declared set) cannot
/// exceed the operator grant either.
fn enforce_policy_capabilities(
    id: &str,
    loaded: &LoadedManifest,
    permitted: Option<&[String]>,
) -> Result<(), LoadRefusal> {
    let Some(permitted) = permitted else {
        return Ok(());
    };
    let permitted_set = || {
        if permitted.is_empty() {
            "none".to_owned()
        } else {
            permitted.join(", ")
        }
    };
    for declared in loaded.dependencies.keys() {
        if !permitted.iter().any(|p| p == declared) {
            return Err(LoadRefusal::CapabilityNotPermitted {
                id: id.to_owned(),
                capability: declared.clone(),
                permitted: permitted_set(),
            });
        }
    }
    // A block or event trigger delivers chain data without an
    // import, so the `chain` grant gates it too.
    let declares_chain_trigger = loaded
        .triggers
        .iter()
        .any(|t| matches!(t, Trigger::Block { .. } | Trigger::Event { .. }));
    if declares_chain_trigger && !permitted.iter().any(|p| p == "chain") {
        return Err(LoadRefusal::ChainTriggerNotPermitted {
            id: id.to_owned(),
            permitted: permitted_set(),
        });
    }
    Ok(())
}

/// A failed `init` loads the module dead; the dispatcher skips it.
pub(super) async fn module<T: RuntimeTypes>(
    shared: &Shared<T>,
    linker: &Linker<HostState<T>>,
    entry: &ModuleEntry,
    loaded_manifest: LoadedManifest,
    resolved: ResolvedLimits,
    env: &BootEnv<'_>,
) -> Result<LoadedModule<T>, RuntimeError> {
    let BootEnv {
        limits: limits_cfg,
        policy,
        require_component_digest,
        ..
    } = *env;
    let module_namespace: ModuleId = manifest_namespace(&loaded_manifest);
    let effective = policy.for_component(&entry.id);
    enforce_policy_capabilities(&entry.id, &loaded_manifest, effective.capabilities)
        .with_refusal_context(|| format!("install refused for {}", entry.path.display()))?;
    let registry = capability_registry(&shared.extensions);
    let sections = &loaded_manifest.extensions;
    let pins = DigestPolicy {
        operator: entry.digest.as_ref(),
        author: loaded_manifest.component_digest.as_ref(),
        require_author: require_component_digest,
    };
    let ((), component, digest) = admit_and_verify(
        shared,
        module_namespace.as_str(),
        &entry.path,
        &loaded_manifest,
        &registry,
        pins,
        || {
            for ext in &shared.extensions {
                ext.admit_worker(module_namespace.as_str(), sections)
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
    } = resolved;
    info!(
        module = %module_namespace,
        id = %entry.id,
        fuel,
        memory_bytes = memory,
        state_bytes,
        "applied module resource limits",
    );
    let spec = StoreSpec {
        http_allowlist: loaded_manifest.http_allowlist.clone(),
        http_operator_allow: effective.http_allow.map(<[_]>::to_vec),
        http_limits: limits_cfg.http,
        http_permitted: limits_cfg.http_permit_destinations.clone(),
        http_denied: policy.http_deny.clone(),
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
        dispatch_deadline: limits_cfg.dispatch_deadline,
    };
    let (run, mut store) =
        fresh_run_store(shared, &module_namespace, 0, &seed.spec).map_err(EngineRefusal::new)?;
    let (bindings, init) = instantiate_module(linker, &seed, &module_namespace, &mut store)
        .await
        .map_err(EngineRefusal::new)?;
    // A failed `init` leaves guest state uninitialized, so the module loads dead.
    let init_succeeded = match init {
        Ok(()) => {
            info!(module = %module_namespace, "init succeeded");
            true
        }
        Err(e) => {
            warn!(
                module = %module_namespace,
                kind = nexum_runtime_wasm::fault_label(&e),
                message = %nexum_runtime_wasm::fault_message(&e),
                "init failed - module loaded but marked dead; dispatcher will skip it",
            );
            false
        }
    };
    // Unserviceable triggers warn; an undeclared extension kind refuses.
    let extension_kinds = extension_trigger_kinds(&shared.extensions);
    for trigger in &loaded_manifest.triggers {
        match trigger {
            Trigger::Schedule { .. } => warn!(
                module = %module_namespace,
                "schedule triggers are declared but never fire until 0.3",
            ),
            Trigger::Extension { extension_kind, .. }
                if !extension_kinds.contains(extension_kind.as_str()) =>
            {
                return Err(LoadRefusal::UnknownTriggerKind {
                    module: module_namespace.clone(),
                    kind: extension_kind.clone(),
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
        triggers: loaded_manifest.triggers.clone(),
        health: Health::from_init(init_succeeded),
    })
}
