//! Everything derived from the wired extensions slice except the linker.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::load::LoadRefusal;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{Extension, HostService, HostServices, ServiceKind};
use crate::manifest::{self, CapabilityRegistry};

/// One registered service kind paired with the service its installs bind to.
pub(super) type ServiceRow<T> = (Box<dyn ServiceKind<T>>, Arc<dyn HostService>);

/// Registered service kinds, keyed by their manifest spelling.
pub(super) type ServiceKinds<T> = BTreeMap<&'static str, ServiceRow<T>>;

/// Refuses a duplicate spelling and a kind whose extension owns no service
/// to install into.
pub(super) fn service_kinds<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
    services: &HostServices,
) -> Result<ServiceKinds<T>, LoadRefusal> {
    let mut kinds = ServiceKinds::new();
    for ext in extensions {
        let Some(kind) = ext.service_kind() else {
            continue;
        };
        let service =
            services
                .raw(ext.namespace())
                .cloned()
                .ok_or_else(|| LoadRefusal::ServicelessKind {
                    namespace: ext.namespace(),
                    kind: kind.kind(),
                })?;
        register_kind(&mut kinds, kind, service)?;
    }
    Ok(kinds)
}

/// Refuses a duplicate manifest spelling.
fn register_kind<T: RuntimeTypes>(
    kinds: &mut ServiceKinds<T>,
    entry: Box<dyn ServiceKind<T>>,
    service: Arc<dyn HostService>,
) -> Result<(), LoadRefusal> {
    let kind = entry.kind();
    if kinds.insert(kind, (entry, service)).is_some() {
        return Err(LoadRefusal::KindRegisteredTwice { kind });
    }
    Ok(())
}

pub(super) fn registered_kinds<T: RuntimeTypes>(kinds: &ServiceKinds<T>) -> Vec<&'static str> {
    kinds.keys().copied().collect()
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

/// Refuses a name two wired extensions both claim: service namespace,
/// subscription kind, or manifest section.
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
        for &kind in ext.subscriptions() {
            if !kinds.insert(kind) {
                return Err(LoadRefusal::SubscriptionKindClaimed { kind });
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
