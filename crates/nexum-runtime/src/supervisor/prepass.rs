//! Everything before any compile: manifest resolution, namespace claims,
//! and the configured-chains gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use alloy_chains::Chain;
use strum::{IntoStaticStr, VariantNames};
use thiserror::Error;
use tracing::{info, warn};

use super::store::{ResolvedLimits, resolve_module_limits};
use crate::engine_config::{EngineConfig, PolicySection};
use crate::error::{RefusalContext as _, RuntimeError};
use crate::manifest::{self, CapabilityRegistry, LoadedManifest, ParseError, Trigger};
use crate::module_id::ModuleId;

/// Refusals before any compile; the wording is operator-pinned.
// `IntoStaticStr`: the snake_case variant name is the `error_kind` label;
// `VariantNames` lets the label-set test enumerate without a value.
#[derive(Debug, Error, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub enum BootRefusal {
    /// Every module derives one keccak local-store namespace from the name,
    /// so a second claimant would alias the first one's state.
    #[error(
        "name {name} is claimed twice: {} and {}; \
         [component].name must be unique across [[modules]]",
        held.display(),
        path.display()
    )]
    NamespaceClaimed {
        /// The claimed name.
        name: String,
        /// The holding entry's path.
        held: PathBuf,
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
        "module {name} declares a trigger on chain {chain_id} but no engine.toml was found \
         (running on defaults, no chains configured); create engine.toml with a \
         [chains.{chain_id}] entry"
    )]
    UnconfiguredChainDefaulted {
        /// The declaring module's `[component].name`.
        name: String,
        /// The chain the trigger names.
        chain_id: u64,
    },
    /// Chain access is an operator grant, so a manifest trigger cannot
    /// widen the `[chains]` set from its side of the boundary.
    #[error(
        "module {name} declares a trigger on chain {chain_id} but engine.toml declares no \
         [chains.{chain_id}] entry; configured chains: {}",
        fmt_chain_ids(configured)
    )]
    UnconfiguredChain {
        /// The declaring module's `[component].name`.
        name: String,
        /// The chain the trigger names.
        chain_id: u64,
        /// The chains engine.toml declares.
        configured: BTreeSet<u64>,
    },
    /// N in-ceiling components can still overcommit the host together;
    /// `[policy.total]` bounds the declared sum.
    #[error(
        "component {id} takes the summed memory reservation to {sum} bytes, \
         over [policy.total].max_memory_bytes = {total}"
    )]
    TotalMemoryExceeded {
        /// The entry whose reservation crossed the cap.
        id: String,
        /// The running sum, this entry included, saturating.
        sum: u64,
        /// The configured aggregate cap.
        total: u64,
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

/// Claimed names, each with the claiming entry's path.
pub(super) type NamespaceLedger = BTreeMap<String, PathBuf>;

/// Claim `name` for `path`, refusing a second claimant.
pub(super) fn claim_namespace(
    ledger: &mut NamespaceLedger,
    name: &str,
    path: &Path,
) -> Result<(), BootRefusal> {
    if let Some(held_path) = ledger.get(name) {
        return Err(BootRefusal::NamespaceClaimed {
            name: name.to_owned(),
            held: held_path.clone(),
            path: path.to_path_buf(),
        });
    }
    ledger.insert(name.to_owned(), path.to_path_buf());
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
) -> Result<LoadedManifest, BootRefusal> {
    match resolve_manifest_path(component, explicit).as_deref() {
        Some(p) if p.exists() => {
            info!(manifest = %p.display(), "loading component manifest");
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

/// Refuse any trigger naming a chain absent from `[chains]`, before any
/// guest code runs.
pub(super) fn enforce_trigger_chains(
    name: &str,
    loaded: &LoadedManifest,
    chains: &ConfiguredChains,
) -> Result<(), BootRefusal> {
    for trigger in &loaded.triggers {
        let (Trigger::Block { chain_id } | Trigger::Event { chain_id, .. }) = trigger else {
            continue;
        };
        if !chains.contains(*chain_id) {
            return Err(unconfigured_chain(name, *chain_id, chains));
        }
    }
    Ok(())
}

pub(super) fn unconfigured_chain(
    name: &str,
    chain_id: u64,
    chains: &ConfiguredChains,
) -> BootRefusal {
    if chains.defaulted {
        return BootRefusal::UnconfiguredChainDefaulted {
            name: name.to_owned(),
            chain_id,
        };
    }
    BootRefusal::UnconfiguredChain {
        name: name.to_owned(),
        chain_id,
        configured: chains.ids.clone(),
    }
}

/// Sum the declared memory reservations against `[policy.total]`, in
/// declaration order, refusing on the entry that crosses the cap. The
/// reservation is the resolved effective limit, so a manifest that
/// narrows its ceiling counts at the narrowed value.
pub(super) fn enforce_total_reservation<'a>(
    policy: &PolicySection,
    reservations: impl IntoIterator<Item = (&'a str, usize)>,
) -> Result<(), BootRefusal> {
    let Some(total) = policy.total.max_memory_bytes else {
        return Ok(());
    };
    let mut sum: u64 = 0;
    for (id, memory) in reservations {
        sum = sum.saturating_add(memory as u64);
        if sum > total.get() as u64 {
            return Err(BootRefusal::TotalMemoryExceeded {
                id: id.to_owned(),
                sum,
                total: total.get() as u64,
            });
        }
    }
    Ok(())
}

/// Every manifest loaded, every name claimed, every triggered chain gated,
/// and the reservation sum bounded, in `engine.toml` order. Limits resolve
/// once here; `load::module` reuses them, so a clamp warns once per field.
pub(super) fn run(
    engine_cfg: &EngineConfig,
    registry: &CapabilityRegistry,
) -> Result<Vec<(LoadedManifest, ResolvedLimits)>, RuntimeError> {
    let mut ledger = NamespaceLedger::new();
    let configured_chains = ConfiguredChains::from_config(engine_cfg);
    let mut manifests = Vec::new();
    for entry in &engine_cfg.modules {
        let loaded = load_required_manifest(&entry.path, entry.manifest.as_deref(), registry)
            .with_refusal_context(|| format!("load module {}", entry.path.display()))?;
        let namespace = manifest_namespace(&loaded);
        claim_namespace(&mut ledger, namespace.as_str(), &entry.path)?;
        enforce_trigger_chains(namespace.as_str(), &loaded, &configured_chains)
            .with_refusal_context(|| format!("load module {}", entry.path.display()))?;
        let limits = resolve_module_limits(
            &entry.id,
            &loaded.resources,
            &engine_cfg.policy.for_component(&entry.id).ceilings,
        );
        manifests.push((loaded, limits));
    }
    enforce_total_reservation(
        &engine_cfg.policy,
        engine_cfg
            .modules
            .iter()
            .zip(&manifests)
            .map(|(entry, (_, limits))| (entry.id.as_str(), limits.memory)),
    )?;
    Ok(manifests)
}
