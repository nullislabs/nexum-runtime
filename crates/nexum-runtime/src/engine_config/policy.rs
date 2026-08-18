use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::num::{NonZeroU64, NonZeroUsize};

use ipnet::IpNet;
use serde::Deserialize;

use nexum_primitives::host_pattern::HostPattern;

use super::error::{EngineConfigError, nonzero_u64, nonzero_usize, zero_field};
use super::{nz_u64, nz_usize};

/// Default fuel budget per dispatch (~1e9 WASM instructions).
const DEFAULT_FUEL_PER_DISPATCH: NonZeroU64 = nz_u64(1_000_000_000);

/// Default linear-memory cap per module store (64 MiB).
const DEFAULT_MEMORY_LIMIT: NonZeroUsize = nz_usize(64 * 1024 * 1024);

/// Default per-module local-store byte quota (50 MiB).
const DEFAULT_STATE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPolicySection {
    max_memory_bytes: Option<usize>,
    max_fuel_per_dispatch: Option<u64>,
    max_state_bytes: Option<u64>,
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    http_deny: Vec<String>,
    #[serde(default)]
    total: RawTotalPolicy,
    #[serde(default)]
    component: BTreeMap<String, RawComponentPolicy>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTotalPolicy {
    max_memory_bytes: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComponentPolicy {
    max_memory_bytes: Option<usize>,
    max_fuel_per_dispatch: Option<u64>,
    max_state_bytes: Option<u64>,
    capabilities: Option<Vec<String>>,
    http_allow: Option<Vec<String>>,
}

/// `[policy]` resolved at load. Every value is an operator grant a
/// manifest narrows and never widens; a component the operator never
/// named gets [`Self::ceilings`] and still counts against [`Self::total`].
#[derive(Debug, Clone, Default)]
pub struct PolicySection {
    /// Ceilings for a component without a `[policy.component]` row.
    pub ceilings: PolicyCeilings,
    /// Capability names any component may declare; `None` permits every
    /// capability the runtime supports.
    pub capabilities: Option<Vec<String>>,
    /// Destination ranges no outbound HTTP request may reach, applied
    /// after every allowlist.
    pub http_deny: Vec<IpNet>,
    /// `[policy.total]`: the aggregate cap over the component set.
    pub total: TotalPolicy,
    /// `[policy.component.<id>]` rows, keyed on `[[modules]].id`.
    pub component: HashMap<String, ComponentPolicy>,
}

impl PolicySection {
    /// The effective policy for the component the operator named `id`;
    /// unset row fields and an absent row fall back to `[policy]`.
    pub fn for_component(&self, id: &str) -> EffectivePolicy<'_> {
        let row = self.component.get(id);
        EffectivePolicy {
            ceilings: PolicyCeilings {
                max_memory_bytes: row
                    .and_then(|r| r.max_memory_bytes)
                    .unwrap_or(self.ceilings.max_memory_bytes),
                max_fuel_per_dispatch: row
                    .and_then(|r| r.max_fuel_per_dispatch)
                    .unwrap_or(self.ceilings.max_fuel_per_dispatch),
                max_state_bytes: row
                    .and_then(|r| r.max_state_bytes)
                    .unwrap_or(self.ceilings.max_state_bytes),
            },
            capabilities: row
                .and_then(|r| r.capabilities.as_deref())
                .or(self.capabilities.as_deref()),
            http_allow: row.and_then(|r| r.http_allow.as_deref()),
        }
    }
}

/// The `[policy]` scalar ceilings; a `[component.resources]` request
/// narrows one and never widens it.
#[derive(Debug, Clone, Copy)]
pub struct PolicyCeilings {
    /// Linear-memory cap in bytes per component store.
    pub max_memory_bytes: NonZeroUsize,
    /// Fuel budget granted per dispatch.
    pub max_fuel_per_dispatch: NonZeroU64,
    /// Local-store on-disk byte quota; zero denies every write.
    pub max_state_bytes: u64,
}

impl Default for PolicyCeilings {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MEMORY_LIMIT,
            max_fuel_per_dispatch: DEFAULT_FUEL_PER_DISPATCH,
            max_state_bytes: DEFAULT_STATE_BYTES,
        }
    }
}

/// `[policy.total]`. Bounds declared reservations at boot, not measured
/// usage.
#[derive(Debug, Clone, Copy, Default)]
pub struct TotalPolicy {
    /// Cap on the summed per-component memory reservations; `None`
    /// leaves the sum unbounded.
    pub max_memory_bytes: Option<NonZeroUsize>,
}

/// One `[policy.component.<id>]` row; each unset field falls back to
/// `[policy]`.
#[derive(Debug, Clone, Default)]
pub struct ComponentPolicy {
    /// Linear-memory ceiling override.
    pub max_memory_bytes: Option<NonZeroUsize>,
    /// Fuel ceiling override.
    pub max_fuel_per_dispatch: Option<NonZeroU64>,
    /// Local-store quota override.
    pub max_state_bytes: Option<u64>,
    /// Capability allowlist override.
    pub capabilities: Option<Vec<String>>,
    /// Operator host allowlist; the effective host set is the manifest's
    /// `hosts` intersected with this, minus `[policy].http_deny`.
    pub http_allow: Option<Vec<HostPattern>>,
}

/// One component's resolved view of `[policy]`.
#[derive(Debug, Clone, Copy)]
pub struct EffectivePolicy<'a> {
    /// Ceilings the component's manifest may narrow.
    pub ceilings: PolicyCeilings,
    /// Permitted capability names; `None` permits every capability.
    pub capabilities: Option<&'a [String]>,
    /// Operator host allowlist; `None` leaves the manifest `hosts` list
    /// as the only name-level gate.
    pub http_allow: Option<&'a [HostPattern]>,
}

fn parse_http_deny(entry: &str) -> Result<IpNet, EngineConfigError> {
    entry
        .parse::<IpNet>()
        .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| EngineConfigError::InvalidHttpDeny {
            entry: entry.to_owned(),
        })
}

pub(super) fn resolve_policy(
    raw: RawPolicySection,
    ids: &HashSet<&str>,
) -> Result<PolicySection, EngineConfigError> {
    let ceilings = PolicyCeilings {
        max_memory_bytes: nonzero_usize(
            "policy.max_memory_bytes",
            raw.max_memory_bytes,
            DEFAULT_MEMORY_LIMIT,
        )?,
        max_fuel_per_dispatch: nonzero_u64(
            "policy.max_fuel_per_dispatch",
            raw.max_fuel_per_dispatch,
            DEFAULT_FUEL_PER_DISPATCH,
        )?,
        // Zero stays legal: a zero quota denies every local-store write,
        // which is an enforceable operator choice.
        max_state_bytes: raw.max_state_bytes.unwrap_or(DEFAULT_STATE_BYTES),
    };
    let http_deny = raw
        .http_deny
        .iter()
        .map(|entry| parse_http_deny(entry))
        .collect::<Result<Vec<_>, _>>()?;
    let total = TotalPolicy {
        max_memory_bytes: raw
            .total
            .max_memory_bytes
            .map(|v| {
                NonZeroUsize::new(v).ok_or_else(|| zero_field("policy.total.max_memory_bytes"))
            })
            .transpose()?,
    };
    let mut component = HashMap::with_capacity(raw.component.len());
    for (id, row) in raw.component {
        if !ids.contains(id.as_str()) {
            return Err(EngineConfigError::UnknownPolicyComponent { id });
        }
        let resolved = ComponentPolicy {
            max_memory_bytes: row
                .max_memory_bytes
                .map(|v| {
                    NonZeroUsize::new(v).ok_or_else(|| {
                        zero_field(&format!("policy.component.{id}.max_memory_bytes"))
                    })
                })
                .transpose()?,
            max_fuel_per_dispatch: row
                .max_fuel_per_dispatch
                .map(|v| {
                    NonZeroU64::new(v).ok_or_else(|| {
                        zero_field(&format!("policy.component.{id}.max_fuel_per_dispatch"))
                    })
                })
                .transpose()?,
            max_state_bytes: row.max_state_bytes,
            capabilities: row.capabilities,
            http_allow: row.http_allow.map(|hosts| {
                hosts
                    .iter()
                    .map(|h| HostPattern::from(h.as_str()))
                    .collect()
            }),
        };
        component.insert(id, resolved);
    }
    Ok(PolicySection {
        ceilings,
        capabilities: raw.capabilities,
        http_deny,
        total,
        component,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_config::{EngineConfig, RawEngineConfig};

    #[test]
    fn policy_component_rows_override_and_fall_back() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[policy]
max_memory_bytes = 1000
capabilities     = ["chain", "http"]

[policy.component.wallet]
max_memory_bytes = 500
http_allow       = ["api.cow.fi"]

[[modules]]
id   = "wallet"
path = "w.wasm"

[[modules]]
id   = "tracker"
path = "t.wasm"
"#,
        )
        .expect("policy rows parse");
        let wallet = cfg.policy.for_component("wallet");
        assert_eq!(wallet.ceilings.max_memory_bytes.get(), 500);
        // Unset row fields fall back to [policy].
        assert_eq!(
            wallet.ceilings.max_fuel_per_dispatch.get(),
            PolicyCeilings::default().max_fuel_per_dispatch.get()
        );
        assert_eq!(
            wallet.capabilities,
            Some(&["chain".to_owned(), "http".to_owned()][..])
        );
        assert_eq!(
            wallet.http_allow,
            Some(&[HostPattern::from("api.cow.fi")][..])
        );
        // An unnamed component gets the [policy] defaults whole.
        let tracker = cfg.policy.for_component("tracker");
        assert_eq!(tracker.ceilings.max_memory_bytes.get(), 1000);
        assert!(tracker.http_allow.is_none());
    }

    #[test]
    fn an_absent_policy_permits_every_capability() {
        let cfg: EngineConfig = toml::from_str("").expect("empty config parses");
        assert!(cfg.policy.for_component("anything").capabilities.is_none());
    }

    #[test]
    fn http_deny_parses_cidr_and_bare_addresses() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[policy]
http_deny = ["169.254.0.0/16", "10.1.2.3", "fc00::/7"]
"#,
        )
        .expect("http_deny parses");
        assert_eq!(cfg.policy.http_deny.len(), 3);
        assert_eq!(cfg.policy.http_deny[1].prefix_len(), 32);
    }

    #[test]
    fn an_http_deny_entry_that_is_not_an_address_refuses() {
        let raw = toml::from_str::<RawEngineConfig>("[policy]\nhttp_deny = [\"api.cow.fi\"]\n")
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("a hostname is not a CIDR block");
        assert!(
            matches!(err, EngineConfigError::InvalidHttpDeny { ref entry } if entry == "api.cow.fi"),
            "{err:?}",
        );
    }
}
