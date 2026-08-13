//! Everything before any compile: manifest resolution, namespace claims,
//! and the configured-chains gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use alloy_chains::Chain;
use strum::{IntoStaticStr, VariantNames};
use thiserror::Error;
use tracing::{info, warn};

use super::role::Role;
use crate::engine_config::EngineConfig;
use crate::manifest::{self, CapabilityRegistry, LoadedManifest, ParseError, Subscription};
use crate::module_id::ModuleId;
use crate::refusal::{Refusal, RefusalContext as _};

/// Refusals before any compile; the wording is operator-pinned.
// `IntoStaticStr`: the snake_case variant name is the `error_kind` label;
// `VariantNames` lets the label-set test enumerate without a value.
#[derive(Debug, Error, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum BootRefusal {
    /// Both roles derive one keccak local-store namespace from the name,
    /// so a second claimant would alias the first one's state.
    #[error(
        "name {name} is claimed twice: {held_role} {} and {role} {}; \
         [component].name must be unique across [[modules]] and [[services]]",
        held.display(),
        path.display()
    )]
    NamespaceClaimed {
        /// The claimed name.
        name: String,
        /// The holding entry's role label.
        held_role: &'static str,
        /// The holding entry's path.
        held: PathBuf,
        /// The second claimant's role label.
        role: &'static str,
        /// The second claimant's path.
        path: PathBuf,
    },
    /// The manifest failed to load or validate; the wrapped class is the
    /// counter label.
    #[error(transparent)]
    Manifest(#[from] ParseError),
    /// Only an explicit path lands here; sibling discovery that finds
    /// nothing is [`Self::ManifestMissing`].
    #[error(
        "manifest {} not found for component {}",
        manifest.display(),
        component.display()
    )]
    ManifestNotFound {
        /// The path the operator or the sibling rule named.
        manifest: PathBuf,
        /// The component the manifest was for.
        component: PathBuf,
    },
    /// No sibling `component.toml` and no explicit path.
    #[error(
        "no component.toml for component {}; ship one next to the component \
         or pass its path explicitly (an empty [dependencies] table grants \
         nothing)",
        component.display()
    )]
    ManifestMissing {
        /// The component without a manifest.
        component: PathBuf,
    },
    /// [`Self::UnconfiguredChain`] for a run on defaults: the fix is
    /// creating engine.toml, not editing it.
    #[error(
        "{noun} {name} subscribes to chain {chain_id} but no engine.toml was found \
         (running on defaults, no chains configured); create engine.toml with a \
         [chains.{chain_id}] entry"
    )]
    UnconfiguredChainDefaulted {
        /// The role label, `module` or `service`.
        noun: &'static str,
        /// The subscriber's `[component].name`.
        name: String,
        /// The chain the subscription names.
        chain_id: u64,
    },
    /// Chain access is an operator grant, so a manifest subscription
    /// cannot widen the `[chains]` set from its side of the boundary.
    #[error(
        "{noun} {name} subscribes to chain {chain_id} but engine.toml declares no \
         [chains.{chain_id}] entry; configured chains: {}",
        fmt_chain_ids(configured)
    )]
    UnconfiguredChain {
        /// The role label, `module` or `service`.
        noun: &'static str,
        /// The subscriber's `[component].name`.
        name: String,
        /// The chain the subscription names.
        chain_id: u64,
        /// The chains engine.toml declares.
        configured: BTreeSet<u64>,
    },
}

/// An empty set reads as `none`, never as an empty list.
fn fmt_chain_ids(ids: &BTreeSet<u64>) -> String {
    if ids.is_empty() {
        return "none".to_owned();
    }
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One ledger spans both roles: they derive the same keccak local-store namespace.
pub(super) type NamespaceLedger = BTreeMap<String, (&'static str, PathBuf)>;

/// Claim `name` for `path`, refusing a second claimant.
pub(super) fn claim_namespace(
    ledger: &mut NamespaceLedger,
    name: &str,
    role: &'static str,
    path: &Path,
) -> Result<(), BootRefusal> {
    if let Some((held_role, held_path)) = ledger.get(name) {
        return Err(BootRefusal::NamespaceClaimed {
            name: name.to_owned(),
            held_role,
            held: held_path.clone(),
            role,
            path: path.to_path_buf(),
        });
    }
    ledger.insert(name.to_owned(), (role, path.to_path_buf()));
    Ok(())
}

/// `[component].name`.
pub(super) fn manifest_namespace(loaded: &LoadedManifest) -> ModuleId {
    loaded.name.clone()
}

/// Missing or unresolved refuses the boot.
pub(super) fn load_required_manifest(
    component: &Path,
    explicit: Option<&Path>,
    registry: &CapabilityRegistry,
    role: &'static str,
) -> Result<LoadedManifest, BootRefusal> {
    match resolve_manifest_path(component, explicit).as_deref() {
        Some(p) if p.exists() => {
            info!(manifest = %p.display(), role, "loading component manifest");
            Ok(manifest::load(p, registry)?)
        }
        // Explicit paths only: sibling discovery requires `.exists()`.
        Some(p) => Err(BootRefusal::ManifestNotFound {
            manifest: p.to_path_buf(),
            component: component.to_path_buf(),
        }),
        None => Err(BootRefusal::ManifestMissing {
            component: component.to_path_buf(),
        }),
    }
}

/// Explicit override, else sibling `component.toml`. A retired name found
/// where the manifest should be is reported rather than ignored, since a
/// silent miss reads as a missing manifest.
fn resolve_manifest_path(component: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let dir = component.parent()?.to_owned();
    let canonical = dir.join("component.toml");
    if canonical.exists() {
        return Some(canonical);
    }
    for retired in ["module.toml", "nexum.toml"] {
        let path = dir.join(retired);
        if path.exists() {
            warn!(
                target: "manifest",
                path = %path.display(),
                "{retired} is not read; the manifest is component.toml (ADR-0016)"
            );
        }
    }
    None
}

/// The operator's `[chains]` set from `engine.toml`.
#[derive(Debug, Clone)]
pub struct ConfiguredChains {
    /// Numeric EIP-155 ids; named `[chains.*]` keys normalize to the same id.
    ids: BTreeSet<u64>,
    /// True when the config is the built-in default (no engine.toml found).
    defaulted: bool,
}

impl ConfiguredChains {
    /// Collect the chain ids the operator configured. Named and numeric
    /// keys normalize to one id, so both spellings match.
    pub fn from_config(cfg: &EngineConfig) -> Self {
        Self {
            ids: cfg.chains.keys().copied().map(Chain::id).collect(),
            defaulted: cfg.defaulted,
        }
    }

    pub(super) fn contains(&self, chain_id: u64) -> bool {
        self.ids.contains(&chain_id)
    }
}

/// Refuse any subscription naming a chain absent from `[chains]`, before any
/// guest code runs.
pub(super) fn enforce_subscriptions(
    role: Role,
    name: &str,
    loaded: &LoadedManifest,
    chains: &ConfiguredChains,
) -> Result<(), BootRefusal> {
    for sub in &loaded.subscriptions {
        let (Subscription::Block { chain_id } | Subscription::ChainLog { chain_id, .. }) = sub
        else {
            continue;
        };
        if !chains.contains(*chain_id) {
            return Err(unconfigured_chain(role, name, *chain_id, chains));
        }
    }
    Ok(())
}

pub(super) fn unconfigured_chain(
    role: Role,
    name: &str,
    chain_id: u64,
    chains: &ConfiguredChains,
) -> BootRefusal {
    let noun = role.label();
    if chains.defaulted {
        return BootRefusal::UnconfiguredChainDefaulted {
            noun,
            name: name.to_owned(),
            chain_id,
        };
    }
    BootRefusal::UnconfiguredChain {
        noun,
        name: name.to_owned(),
        chain_id,
        configured: chains.ids.clone(),
    }
}

/// Every manifest loaded, every name claimed, every subscribed chain gated,
/// in `engine.toml` order.
pub(super) struct Prepass {
    pub(super) adapter_manifests: Vec<LoadedManifest>,
    pub(super) module_manifests: Vec<LoadedManifest>,
}

/// Services first, then modules; every refusal lands before any compile.
pub(super) fn run(
    engine_cfg: &EngineConfig,
    registry: &CapabilityRegistry,
) -> Result<Prepass, Refusal> {
    let service_registry = CapabilityRegistry::service();
    let mut ledger = NamespaceLedger::new();
    let configured_chains = ConfiguredChains::from_config(engine_cfg);
    let adapter_manifests = load_role_manifests(
        engine_cfg
            .services
            .iter()
            .map(|e| (&e.path, e.manifest.as_deref())),
        &service_registry,
        RolePass {
            role: Role::Service,
            chains: &configured_chains,
        },
        &mut ledger,
    )?;
    let module_manifests = load_role_manifests(
        engine_cfg
            .modules
            .iter()
            .map(|e| (&e.path, e.manifest.as_deref())),
        registry,
        RolePass {
            role: Role::Module,
            chains: &configured_chains,
        },
        &mut ledger,
    )?;
    Ok(Prepass {
        adapter_manifests,
        module_manifests,
    })
}

struct RolePass<'a> {
    role: Role,
    chains: &'a ConfiguredChains,
}

/// In declaration order.
fn load_role_manifests<'a>(
    entries: impl Iterator<Item = (&'a PathBuf, Option<&'a Path>)>,
    registry: &CapabilityRegistry,
    pass: RolePass<'_>,
    ledger: &mut NamespaceLedger,
) -> Result<Vec<LoadedManifest>, Refusal> {
    let mut manifests = Vec::new();
    for (path, explicit) in entries {
        let loaded = load_required_manifest(path, explicit, registry, pass.role.label())
            .with_refusal_context(|| format!("{} {}", pass.role.load_context(), path.display()))?;
        let namespace = manifest_namespace(&loaded);
        claim_namespace(ledger, namespace.as_str(), pass.role.label(), path)?;
        enforce_subscriptions(pass.role, namespace.as_str(), &loaded, pass.chains)
            .with_refusal_context(|| format!("{} {}", pass.role.load_context(), path.display()))?;
        manifests.push(loaded);
    }
    Ok(manifests)
}
