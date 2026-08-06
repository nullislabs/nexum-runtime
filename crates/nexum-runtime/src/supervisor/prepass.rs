//! Everything before any compile: manifest resolution, namespace claims,
//! and the configured-chains gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use alloy_chains::Chain;
use anyhow::{Context, Error, Result, anyhow};
use tracing::{info, warn};

use super::role::Role;
use crate::engine_config::EngineConfig;
use crate::manifest::{self, CapabilityRegistry, LoadedManifest, Subscription};

/// One ledger spans both roles: they derive the same keccak local-store namespace.
pub(super) type NamespaceLedger = BTreeMap<String, (&'static str, PathBuf)>;

/// Claim `name` for `path`, refusing a second claimant.
pub(super) fn claim_namespace(
    ledger: &mut NamespaceLedger,
    name: &str,
    role: &'static str,
    path: &Path,
) -> Result<()> {
    if let Some((held_role, held_path)) = ledger.get(name) {
        return Err(anyhow!(
            "name {name} is claimed twice: {held_role} {} and {role} {}; \
             [module].name must be unique across [[modules]] and [[adapters]]",
            held_path.display(),
            path.display(),
        ));
    }
    ledger.insert(name.to_owned(), (role, path.to_path_buf()));
    Ok(())
}

/// `[module].name`; manifest parse already refused a blank one.
pub(super) fn manifest_namespace(loaded: &LoadedManifest) -> String {
    loaded.manifest.module.name.clone()
}

/// Missing or unresolved refuses the boot.
pub(super) fn load_required_manifest(
    component: &Path,
    explicit: Option<&Path>,
    registry: &CapabilityRegistry,
    role: &'static str,
) -> Result<LoadedManifest> {
    match resolve_manifest_path(component, explicit).as_deref() {
        Some(p) if p.exists() => {
            info!(manifest = %p.display(), role, "loading component manifest");
            Ok(manifest::load(p, registry)?)
        }
        // Explicit paths only: sibling discovery requires `.exists()`.
        Some(p) => Err(anyhow!(
            "manifest {} not found for component {}",
            p.display(),
            component.display(),
        )),
        None => Err(anyhow!(
            "no module.toml for component {}; ship one next to the component \
             or pass its path explicitly (an empty `required = []` under \
             [capabilities] grants nothing)",
            component.display(),
        )),
    }
}

/// Explicit override, else sibling `module.toml`, else deprecated
/// `nexum.toml` with a rename warning; `None` when neither exists.
fn resolve_manifest_path(component: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let dir = component.parent()?.to_owned();
    let canonical = dir.join("module.toml");
    if canonical.exists() {
        return Some(canonical);
    }
    let legacy = dir.join("nexum.toml");
    if legacy.exists() {
        warn!(
            target: "manifest",
            path = %legacy.display(),
            "nexum.toml is deprecated; rename to module.toml \
             (ADR-0001). Support will be removed in 0.3."
        );
        return Some(legacy);
    }
    None
}

/// The operator's `[chains]` set from `engine.toml`.
#[derive(Debug, Clone)]
pub struct ConfiguredChains {
    /// Numeric EIP-155 ids; named `[chains.*]` keys normalise to the same id.
    ids: BTreeSet<u64>,
    /// True when the config is the built-in default (no engine.toml found).
    defaulted: bool,
}

impl ConfiguredChains {
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

/// Refuse any subscription naming a chain absent from `[chains]`, before any guest code runs.
pub(super) fn enforce_configured_chains(
    module: &str,
    loaded: &LoadedManifest,
    chains: &ConfiguredChains,
) -> Result<()> {
    for sub in &loaded.manifest.subscriptions {
        let (Subscription::Block { chain_id } | Subscription::ChainLog { chain_id, .. }) = sub
        else {
            continue;
        };
        if !chains.contains(*chain_id) {
            return Err(unconfigured_chain(module, *chain_id, chains));
        }
    }
    Ok(())
}

pub(super) fn unconfigured_chain(module: &str, chain_id: u64, chains: &ConfiguredChains) -> Error {
    if chains.defaulted {
        return anyhow!(
            "module {module} subscribes to chain {chain_id} but no engine.toml was found \
             (running on defaults, no chains configured); create engine.toml with a \
             [chains.{chain_id}] entry"
        );
    }
    let configured = if chains.ids.is_empty() {
        "none".to_owned()
    } else {
        chains
            .ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    anyhow!(
        "module {module} subscribes to chain {chain_id} but engine.toml declares no \
         [chains.{chain_id}] entry; configured chains: {configured}"
    )
}

/// Every manifest loaded, every name claimed, every subscribed chain gated,
/// in `engine.toml` order.
pub(super) struct Prepass {
    pub(super) adapter_manifests: Vec<LoadedManifest>,
    pub(super) module_manifests: Vec<LoadedManifest>,
}

/// Adapters first, then modules; every refusal lands before any compile.
pub(super) fn run(engine_cfg: &EngineConfig, registry: &CapabilityRegistry) -> Result<Prepass> {
    let provider_registry = CapabilityRegistry::provider();
    let mut ledger = NamespaceLedger::new();
    let configured_chains = ConfiguredChains::from_config(engine_cfg);
    let adapter_manifests = load_role_manifests(
        engine_cfg
            .adapters
            .iter()
            .map(|e| (&e.path, e.manifest.as_deref())),
        &provider_registry,
        RolePass {
            role: Role::Adapter,
            chains: None,
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
            chains: Some(&configured_chains),
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
    chains: Option<&'a ConfiguredChains>,
}

/// In declaration order.
fn load_role_manifests<'a>(
    entries: impl Iterator<Item = (&'a PathBuf, Option<&'a Path>)>,
    registry: &CapabilityRegistry,
    pass: RolePass<'_>,
    ledger: &mut NamespaceLedger,
) -> Result<Vec<LoadedManifest>> {
    let mut manifests = Vec::new();
    for (path, explicit) in entries {
        let loaded = load_required_manifest(path, explicit, registry, pass.role.manifest_role())
            .with_context(|| format!("{} {}", pass.role.load_context(), path.display()))?;
        let namespace = manifest_namespace(&loaded);
        claim_namespace(ledger, &namespace, pass.role.claim_role(), path)?;
        if let Some(chains) = pass.chains {
            enforce_configured_chains(&namespace, &loaded, chains)
                .with_context(|| format!("{} {}", pass.role.load_context(), path.display()))?;
        }
        manifests.push(loaded);
    }
    Ok(manifests)
}
