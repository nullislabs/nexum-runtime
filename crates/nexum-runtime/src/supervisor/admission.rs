//! Everything derived from the wired extensions slice except the linker.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::host::component::RuntimeTypes;
use crate::host::extension::{Extension, HostService, HostServices, ProviderKind};
use crate::manifest::{self, CapabilityRegistry};

/// One registered provider kind paired with the service its installs bind to.
pub(super) type ProviderRow<T> = (Box<dyn ProviderKind<T>>, Arc<dyn HostService>);

/// Registered provider kinds, keyed by their manifest spelling.
pub(super) type ProviderKinds<T> = BTreeMap<&'static str, ProviderRow<T>>;

/// Refuses a duplicate spelling and a kind whose extension owns no service
/// to install into.
pub(super) fn provider_kinds<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
    services: &HostServices,
) -> Result<ProviderKinds<T>> {
    let mut kinds = ProviderKinds::new();
    for ext in extensions {
        let Some(provider) = ext.provider() else {
            continue;
        };
        let service = services.raw(ext.namespace()).cloned().ok_or_else(|| {
            anyhow!(
                "extension {} registers provider kind {} without a host service",
                ext.namespace(),
                provider.kind(),
            )
        })?;
        register_kind(&mut kinds, provider, service)?;
    }
    Ok(kinds)
}

/// Refuses a duplicate manifest spelling.
fn register_kind<T: RuntimeTypes>(
    kinds: &mut ProviderKinds<T>,
    provider: Box<dyn ProviderKind<T>>,
    service: Arc<dyn HostService>,
) -> Result<()> {
    let kind = provider.kind();
    if kinds.insert(kind, (provider, service)).is_some() {
        return Err(anyhow!("provider kind {kind} is registered twice"));
    }
    Ok(())
}

pub(super) fn registered_kinds<T: RuntimeTypes>(kinds: &ProviderKinds<T>) -> String {
    kinds.keys().copied().collect::<Vec<_>>().join(", ")
}

pub(super) fn extension_subscription_vocabulary<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
) -> BTreeSet<&'static str> {
    extensions
        .iter()
        .flat_map(|ext| ext.subscriptions().iter().copied())
        .collect()
}

/// Refuse a manifest section no wired extension claims.
pub(super) fn enforce_extension_sections<T: RuntimeTypes>(
    owner: &str,
    sections: &manifest::ExtensionSections,
    extensions: &[Arc<dyn Extension<T>>],
) -> Result<()> {
    for key in sections.keys() {
        let claimed = extensions
            .iter()
            .any(|ext| ext.manifest_sections().contains(&key.as_str()));
        if !claimed {
            return Err(anyhow!(
                "{owner} declares manifest section [{key}]; no wired extension claims it"
            ));
        }
    }
    Ok(())
}

/// Refuses a name two wired extensions both claim: service namespace,
/// subscription kind, or manifest section.
pub(super) fn enforce_extension_uniqueness<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
) -> Result<()> {
    let mut namespaces = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut sections = BTreeSet::new();
    for ext in extensions {
        let namespace = ext.namespace();
        if !namespaces.insert(namespace) {
            return Err(anyhow!("extension namespace {namespace} is claimed twice"));
        }
        for kind in ext.subscriptions() {
            if !kinds.insert(*kind) {
                return Err(anyhow!("subscription kind {kind} is claimed twice"));
            }
        }
        for section in ext.manifest_sections() {
            if !sections.insert(*section) {
                return Err(anyhow!("manifest section [{section}] is claimed twice"));
            }
        }
    }
    Ok(())
}

/// Must agree with the linker built from the same `extensions`.
pub(super) fn capability_registry<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
) -> CapabilityRegistry {
    let mut registry = CapabilityRegistry::core();
    for ext in extensions {
        registry.register(ext.capabilities());
    }
    registry
}
