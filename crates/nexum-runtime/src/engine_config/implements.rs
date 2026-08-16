//! `[implements]`: the operator's binding of an interface track to its
//! authorized implementer.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use super::error::EngineConfigError;
use crate::digest::ContentDigest;
use crate::interface_id::InterfaceTrack;

/// `[implements]` resolved at load, keyed on the compatibility track. A
/// `provides` claim with no row here does not load: type-level truth is
/// not authorization (ADR-0001).
pub type ImplementsSection = BTreeMap<InterfaceTrack, Implementer>;

/// One `[implements]` row.
#[derive(Debug, Clone)]
pub struct Implementer {
    /// The authorized `[[modules]].id`. The author-supplied
    /// `[component].name` never binds (ADR-0001).
    pub component: String,
    /// Pin of the implementer's artifact; an unpinned row refuses at load.
    pub digest: Option<ContentDigest>,
}

/// Raw `[implements]` row; validated by [`resolve_implements`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawImplementer {
    pub(super) component: String,
    #[serde(default)]
    pub(super) digest: Option<String>,
}

pub(super) fn resolve_implements(
    raw: BTreeMap<String, RawImplementer>,
    ids: &HashSet<&str>,
) -> Result<ImplementsSection, EngineConfigError> {
    let mut implements = ImplementsSection::new();
    for (key, row) in raw {
        let Ok(track) = InterfaceTrack::parse(&key) else {
            return Err(EngineConfigError::InvalidInterfaceTrack { key });
        };
        if !ids.contains(row.component.as_str()) {
            return Err(EngineConfigError::UnknownImplementsComponent {
                interface: key,
                id: row.component,
            });
        }
        let digest = match row.digest {
            Some(value) => Some(value.parse::<ContentDigest>().map_err(|source| {
                EngineConfigError::InvalidImplementerDigest {
                    interface: key,
                    value,
                    source,
                }
            })?),
            None => None,
        };
        implements.insert(
            track,
            Implementer {
                component: row.component,
                digest,
            },
        );
    }
    Ok(implements)
}
