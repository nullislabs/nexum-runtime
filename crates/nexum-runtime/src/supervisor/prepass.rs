//! Everything before any compile: manifest resolution and loading,
//! namespace claims, and the configured-chains gate. Every refusal here
//! fires before a single component byte is read or compiled.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use alloy_chains::Chain;
use anyhow::{Context, Error, Result, anyhow};
use tracing::{info, warn};

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

/// Fallback namespace for a module with an empty `[module].name`.
pub(super) const MODULE_FALLBACK_NAME: &str = "module";

/// Fallback namespace for a provider with an empty `[module].name`.
pub(super) const PROVIDER_FALLBACK_NAME: &str = "provider";

/// `[module].name`, or `fallback` when it is empty.
pub(super) fn manifest_namespace(loaded: &LoadedManifest, fallback: &str) -> String {
    if loaded.manifest.module.name.is_empty() {
        fallback.to_owned()
    } else {
        loaded.manifest.module.name.clone()
    }
}

/// Load the mandatory manifest for `component`; missing or unresolved
/// refuses the boot.
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

/// Resolve a component's manifest: explicit override, else sibling
/// `module.toml`, else the deprecated `nexum.toml` with a rename warning.
/// `None` when neither exists.
fn resolve_manifest_path(component: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    // Canonical name is module.toml (ADR-0001). nexum.toml is accepted
    // with a deprecation warning during the 0.1->0.2 transition.
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

/// The operator's configured chain set from `[chains]` in `engine.toml`.
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

/// Boot error for an unconfigured chain subscription.
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

/// The pre-pass output: every manifest loaded, every name claimed, and
/// every subscribed chain gated, in `engine.toml` order.
pub(super) struct Prepass {
    pub(super) adapter_manifests: Vec<LoadedManifest>,
    pub(super) module_manifests: Vec<LoadedManifest>,
}

/// Run the pre-pass over `engine_cfg`: adapters first, then modules, so a
/// refusal lands before any compile in either role.
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
            manifest_role: "provider",
            claim_role: "adapter",
            context: "load provider",
            fallback: PROVIDER_FALLBACK_NAME,
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
            manifest_role: "module",
            claim_role: "module",
            context: "load module",
            fallback: MODULE_FALLBACK_NAME,
            chains: Some(&configured_chains),
        },
        &mut ledger,
    )?;
    Ok(Prepass {
        adapter_manifests,
        module_manifests,
    })
}

/// One role's pre-pass parameters: manifest-load role, ledger claim role,
/// error context, fallback namespace, and the optional chains gate.
struct RolePass<'a> {
    manifest_role: &'static str,
    claim_role: &'static str,
    context: &'static str,
    fallback: &'static str,
    chains: Option<&'a ConfiguredChains>,
}

/// Load, claim, and gate every entry of one role, in declaration order.
fn load_role_manifests<'a>(
    entries: impl Iterator<Item = (&'a PathBuf, Option<&'a Path>)>,
    registry: &CapabilityRegistry,
    pass: RolePass<'_>,
    ledger: &mut NamespaceLedger,
) -> Result<Vec<LoadedManifest>> {
    let mut manifests = Vec::new();
    for (path, explicit) in entries {
        let loaded = load_required_manifest(path, explicit, registry, pass.manifest_role)
            .with_context(|| format!("{} {}", pass.context, path.display()))?;
        let namespace = manifest_namespace(&loaded, pass.fallback);
        claim_namespace(ledger, &namespace, pass.claim_role, path)?;
        if let Some(chains) = pass.chains {
            enforce_configured_chains(&namespace, &loaded, chains)
                .with_context(|| format!("{} {}", pass.context, path.display()))?;
        }
        manifests.push(loaded);
    }
    Ok(manifests)
}
