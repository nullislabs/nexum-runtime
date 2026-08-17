//! Everything derived from the wired extensions slice except the linker.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::load::LoadRefusal;
use crate::host::component::RuntimeTypes;
use crate::host::extension::Extension;
use crate::manifest::{self, CapabilityRegistry};

pub(super) fn extension_trigger_kinds<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
) -> BTreeSet<&'static str> {
    extensions
        .iter()
        .flat_map(|ext| ext.emits_trigger_kinds().iter().copied())
        .collect()
}

/// Refuse a manifest section no wired extension claims.
pub(super) fn enforce_extension_sections<T: RuntimeTypes>(
    owner: &str,
    sections: &manifest::ExtensionSections,
    extensions: &[Arc<dyn Extension<T>>],
) -> Result<(), LoadRefusal> {
    for key in sections.keys() {
        let claimed = extensions
            .iter()
            .any(|ext| ext.manifest_sections().contains(&key.as_str()));
        if !claimed {
            return Err(LoadRefusal::SectionUnclaimed {
                owner: owner.to_owned(),
                section: key.clone(),
            });
        }
    }
    Ok(())
}

/// Refuses a name two wired extensions both claim: namespace, trigger
/// kind, or manifest section.
pub(super) fn enforce_extension_uniqueness<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
) -> Result<(), LoadRefusal> {
    let mut namespaces = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut sections = BTreeSet::new();
    for ext in extensions {
        let namespace = ext.namespace();
        if !namespaces.insert(namespace) {
            return Err(LoadRefusal::ExtensionNamespaceClaimed { namespace });
        }
        for &kind in ext.emits_trigger_kinds() {
            if !kinds.insert(kind) {
                return Err(LoadRefusal::TriggerKindClaimed { kind });
            }
        }
        for &section in ext.manifest_sections() {
            if !sections.insert(section) {
                return Err(LoadRefusal::SectionClaimed { section });
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
