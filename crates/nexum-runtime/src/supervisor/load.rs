//! Load one module: admission, verified compile, instantiation, `init`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Context, Error, Result};
use strum::{IntoStaticStr, VariantNames};
use thiserror::Error as ThisError;
use tracing::{info, warn};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker};

use super::admission::{
    capability_registry, enforce_extension_sections, extension_subscription_vocabulary,
};
use super::artifact::{DigestPolicy, read_verified_component};
use super::dispatch::with_dispatch_deadline;
use super::lifecycle::Health;
use super::prepass::manifest_namespace;
use super::store::{HostStore, ResolvedLimits, StoreSpec, fresh_run_store};
use super::{BootEnv, Shared};
use crate::bindings::nexum::host::types::Fault;
use crate::bindings::{Config, EventModule};
use crate::digest::ContentDigest;
use crate::engine_config::{ImplementsSection, ModuleEntry};
use crate::host::component::RuntimeTypes;
use crate::host::logs::RunId;
use crate::host::state::HostState;
use crate::interface_id::{InterfaceId, InterfaceTrack};
use crate::manifest::{self, CapabilityRegistry, LoadedManifest, Subscription};
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
    /// An embedder wiring bug: an extension's namespace must have one
    /// owner.
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
    /// Chain events reach the guest through `on_event`, not an import, so
    /// the subscription is gated on the same operator grant as the `chain`
    /// dependency.
    #[error(
        "component {id} subscribes to chain events; \
         [policy].capabilities permits only: {permitted}"
    )]
    ChainSubscriptionNotPermitted {
        /// The entry's operator-written id.
        id: String,
        /// The permitted set, for the fix.
        permitted: String,
    },
    /// The `provides` claim is author-supplied (ADR-0001); a component
    /// that does not export it must not enter the tree as its implementer.
    #[error(
        "component {id} ({}) claims provides = {claimed} but exports no \
         satisfying interface instance; interface exports: {exported}",
        path.display()
    )]
    ProvidesNotExported {
        /// The entry's operator-written id.
        id: String,
        /// The artifact whose exports were walked.
        path: PathBuf,
        /// The claimed interface id.
        claimed: String,
        /// The interface-instance exports found, so a version near-miss
        /// is legible; `none` when there are none.
        exported: String,
    },
    /// A genuine export is still not authorization: binding an
    /// implementer is an operator act, written in `[implements]`
    /// (ADR-0001), and there is no permissive default.
    #[error(
        "component {id} provides {interface} but [implements].\"{interface}\" \
         authorizes: {bound}; bind the interface to this entry's [[modules]].id \
         in engine.toml"
    )]
    ImplementerUnbound {
        /// The entry's operator-written id.
        id: String,
        /// The claim's compatibility track, as the `[implements]` key.
        interface: String,
        /// What the row authorizes today: another id, or `nothing`.
        bound: String,
    },
    /// The binding names an id, not bytes; only the digest fixes the
    /// artifact, so an unpinned implementer does not load.
    #[error(
        "component {id} is bound to {interface} without a digest; \
         pin the artifact's sha256 in [implements].\"{interface}\""
    )]
    ImplementerUnpinned {
        /// The entry's operator-written id.
        id: String,
        /// The claim's compatibility track, as the `[implements]` key.
        interface: String,
    },
    /// The row's digest is the only operator-written pin on the artifact.
    /// Were an unmatched row inert, deleting one line of the untrusted
    /// manifest would delete the operator's pin with it.
    #[error(
        "[implements].\"{interface}\" authorizes component {id}, whose manifest \
         provides {claimed}; the row pins no artifact as written, so drop it or \
         restore the claim"
    )]
    ImplementerNotClaiming {
        /// The entry's operator-written id.
        id: String,
        /// The row's key, which the entry does not claim.
        interface: String,
        /// The entry's claim as a track, or `nothing`.
        claimed: String,
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

/// Every refusal precedes compile cost.
fn admit_and_verify<T: RuntimeTypes, R>(
    shared: &Shared<T>,
    owner: &str,
    path: &Path,
    loaded_manifest: &LoadedManifest,
    registry: &CapabilityRegistry,
    pins: DigestPolicy<'_>,
    admit: impl FnOnce() -> Result<R, Refusal>,
) -> Result<(R, Component, ContentDigest), Refusal> {
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
        seed.dispatch_deadline,
    )
    .await?;
    Ok((bindings, init))
}

/// `[policy].capabilities` bounds what a manifest may declare, so the
/// component's imports (already checked against the declared set) cannot
/// exceed the operator grant either. Interface dependencies are outside
/// it: the operator authorizes a provided interface through the
/// provider's `[implements]` row, not through the consumer's grant
/// (ADR-0018, amended).
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
    // A block or chain-log subscription delivers chain data without an
    // import, so the `chain` grant gates it too.
    let subscribes_to_chain = loaded.subscriptions.iter().any(|sub| {
        matches!(
            sub,
            Subscription::Block { .. } | Subscription::ChainLog { .. }
        )
    });
    if subscribes_to_chain && !permitted.iter().any(|p| p == "chain") {
        return Err(LoadRefusal::ChainSubscriptionNotPermitted {
            id: id.to_owned(),
            permitted: permitted_set(),
        });
    }
    Ok(())
}

/// A `provides` claim loads only for the entry `[implements]` binds and
/// pins; the returned pin is verified on the exact bytes the compiler
/// receives. `None` claim is the common case and needs no row.
///
/// The sweep over every row runs first, and it is what stops the author
/// from switching the operator's pin off: a row naming this entry must be
/// matched by the entry's claim, so deleting the manifest's `provides`
/// line refuses instead of dropping the operator's digest with it.
fn enforce_implements<'a>(
    entry: &ModuleEntry,
    claim: Option<&InterfaceId>,
    implements: &'a ImplementsSection,
) -> Result<Option<&'a ContentDigest>, LoadRefusal> {
    let claimed = claim.map(InterfaceId::track);
    for (row_track, row) in implements {
        if row.component == entry.id && Some(row_track) != claimed.as_ref() {
            return Err(LoadRefusal::ImplementerNotClaiming {
                id: entry.id.clone(),
                interface: row_track.to_string(),
                claimed: claimed
                    .as_ref()
                    .map_or_else(|| "nothing".to_owned(), InterfaceTrack::to_string),
            });
        }
    }
    let Some(track) = claimed else {
        return Ok(None);
    };
    let row = implements.get(&track);
    // A row alone is a presence test; the binding is the id comparison.
    let Some(row) = row.filter(|row| row.component == entry.id) else {
        return Err(LoadRefusal::ImplementerUnbound {
            id: entry.id.clone(),
            interface: track.to_string(),
            bound: row.map_or_else(|| "nothing".to_owned(), |row| row.component.clone()),
        });
    };
    let Some(digest) = row.digest.as_ref() else {
        return Err(LoadRefusal::ImplementerUnpinned {
            id: entry.id.clone(),
            interface: track.to_string(),
        });
    };
    Ok(Some(digest))
}

/// The claim is satisfied only by an interface-instance export: a bare
/// func under a matching name must not pass, and `synthesize` gives every
/// module `init` and `on-event` funcs.
///
/// The match is nominal: name, kind and version, never the interface's
/// surface, so an empty instance under the claimed name passes. A
/// consumer's dependency (#205) carries only a track, so nothing in the
/// engine holds the interface's WIT to compare against until the call
/// wiring lands (#206), which is also when a caller could first be
/// misled by the gap.
pub(super) fn enforce_provides<'a>(
    id: &str,
    path: &Path,
    claim: &InterfaceId,
    exports: impl Iterator<Item = (&'a str, ComponentItem)>,
) -> Result<(), LoadRefusal> {
    let mut instance_exports = Vec::new();
    for (name, item) in exports {
        if !matches!(item, ComponentItem::ComponentInstance(_)) {
            continue;
        }
        if claim.matches_export(name) {
            return Ok(());
        }
        instance_exports.push(name.to_owned());
    }
    Err(LoadRefusal::ProvidesNotExported {
        id: id.to_owned(),
        path: path.to_path_buf(),
        claimed: claim.to_string(),
        exported: if instance_exports.is_empty() {
            "none".to_owned()
        } else {
            instance_exports.join(", ")
        },
    })
}

/// A failed `init` loads the module dead; the dispatcher skips it.
pub(super) async fn module<T: RuntimeTypes>(
    shared: &Shared<T>,
    linker: &Linker<HostState<T>>,
    entry: &ModuleEntry,
    loaded_manifest: LoadedManifest,
    resolved: ResolvedLimits,
    env: &BootEnv<'_>,
) -> Result<LoadedModule<T>, Refusal> {
    let BootEnv {
        limits: limits_cfg,
        policy,
        implements,
        require_component_digest,
        ..
    } = *env;
    let module_namespace: ModuleId = manifest_namespace(&loaded_manifest);
    let effective = policy.for_component(&entry.id);
    enforce_policy_capabilities(&entry.id, &loaded_manifest, effective.capabilities)
        .with_refusal_context(|| format!("install refused for {}", entry.path.display()))?;
    // Before any artifact byte is read: authorization precedes verification.
    let operator_pin = enforce_implements(entry, loaded_manifest.provides.as_ref(), implements)
        .with_refusal_context(|| format!("install refused for {}", entry.path.display()))?;
    let registry = capability_registry(&shared.extensions);
    let sections = &loaded_manifest.extensions;
    let pins = DigestPolicy {
        operator: operator_pin,
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
    // Post-compile, pre-instantiation, exactly as the import walk above:
    // a false claim never reaches `instantiate` or `init`.
    if let Some(claim) = &loaded_manifest.provides {
        enforce_provides(
            &entry.id,
            &entry.path,
            claim,
            component
                .component_type()
                .exports(&shared.engine)
                .map(|(name, export)| (name, export.ty)),
        )
        .with_refusal_context(|| format!("install refused for {}", entry.path.display()))?;
    }

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
    let (run, mut store) = fresh_run_store(shared, &module_namespace, 0, &seed.spec)?;
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
                kind = crate::host::fault::fault_label(&e),
                message = %crate::host::fault::fault_message(&e),
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
