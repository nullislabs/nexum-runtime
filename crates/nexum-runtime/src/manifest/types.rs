//! Raw serde shapes (`Manifest` and its sections) and the validated
//! `LoadedManifest` they convert into. Deserialization only decides that
//! the TOML is well formed; `TryFrom<Manifest>` runs every value check
//! and returns a typed [`ParseError`].

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256};
use serde::Deserialize;

use super::error::ParseError;
use crate::host_pattern::HostPattern;

/// Core capability names: the `nexum:host` interfaces linked into every
/// module. `http` is gated separately (it gates `wasi:http/*`), and
/// extensions register their own namespaces.
pub const CORE_CAPABILITIES: &[&str] = &nexum_world::CORE_IFACES;

/// Raw deserialized manifest; every value stays as written until the
/// `TryFrom<Manifest>` conversion into [`LoadedManifest`] validates it.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct Manifest {
    #[serde(default)]
    pub component: ComponentSection,
    #[serde(default)]
    pub dependencies: Option<DependencySection>,
    #[serde(default)]
    pub config: toml::Table,
    /// `[[subscription]]` tables as written; parsed by the validation
    /// pass.
    #[serde(default, rename = "subscription")]
    pub subscriptions: Vec<toml::Table>,
    /// Extension-owned sections (every non-core top-level key), parsed
    /// opaquely and routed to the wired extensions; a section no extension
    /// claims is refused at boot.
    #[serde(flatten)]
    pub extensions: ExtensionSections,
}

/// Extension-owned manifest sections, keyed by top-level name. Opaque
/// to the runtime; each claiming extension parses its own.
pub type ExtensionSections = BTreeMap<String, toml::Value>;

/// One `[[subscription]]` table. The `kind` field discriminates; an
/// unknown kind parses as [`Subscription::Extension`] and is validated at
/// boot against the wired extensions' declared kinds.
#[derive(Debug, Clone)]
pub enum Subscription {
    /// New-block events; one subscription per chain id, fanned out to every
    /// module watching that chain.
    Block {
        /// EVM chain id.
        chain_id: u64,
    },
    /// Chain-log events matching `address` + topic-0; one subscription per
    /// entry, tagged with the owning module. A re-open replays its start
    /// height; `removed` retraction covers only the last delivered
    /// log-bearing height.
    ChainLog {
        /// EVM chain id.
        chain_id: u64,
        /// Contract address filter, declared as 20-byte hex.
        address: Option<Address>,
        /// Topic-0 filter, declared as 32-byte hex; absent matches every
        /// event from the address(es).
        event_signature: Option<B256>,
        /// Persist a durable cursor; a restart re-opens AT the cursor block
        /// and replays it.
        resume: bool,
        /// Backfill cap in blocks for a `resume` subscription; `None`
        /// backfills the whole gap, a cap drops the oldest missed blocks.
        max_lookback: Option<u64>,
    },
    /// Cron-scheduled tick; parsed but not dispatched (the supervisor
    /// warns).
    Cron {
        /// Standard 5-field cron expression.
        #[allow(dead_code)]
        schedule: String,
    },
    /// An extension-owned event kind. Delivered when the kind matches and
    /// every filter pair is present in the event's attributes.
    Extension {
        /// The extension-declared subscription kind.
        kind: String,
        /// Attribute filters; empty admits every event of the kind.
        filters: BTreeMap<String, String>,
    },
}

/// Core subscription kinds shaped by serde; the hex fields stay raw
/// strings until the [`Subscription`] conversion validates them.
// `kebab-case` reproduces `nexum_world::SubscriptionKind`, which gates this.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum CoreSubscription {
    Block {
        chain_id: u64,
    },
    ChainLog {
        chain_id: u64,
        #[serde(default)]
        address: Option<String>,
        #[serde(default)]
        event_signature: Option<String>,
        #[serde(default)]
        resume: bool,
        #[serde(default)]
        max_lookback: Option<u64>,
    },
    Cron {
        schedule: String,
    },
}

impl TryFrom<CoreSubscription> for Subscription {
    type Error = ParseError;

    /// Validates the hex filters; the wording is operator-pinned.
    fn try_from(sub: CoreSubscription) -> Result<Self, ParseError> {
        Ok(match sub {
            CoreSubscription::Block { chain_id } => Self::Block { chain_id },
            CoreSubscription::ChainLog {
                chain_id,
                address,
                event_signature,
                resume,
                max_lookback,
            } => Self::ChainLog {
                chain_id,
                address: address
                    .map(|raw| {
                        raw.parse::<Address>().map_err(|source| {
                            ParseError::InvalidChainLogAddress { value: raw, source }
                        })
                    })
                    .transpose()?,
                event_signature: event_signature
                    .map(|raw| {
                        raw.parse::<B256>()
                            .map_err(|source| ParseError::InvalidChainLogTopic {
                                value: raw,
                                source,
                            })
                    })
                    .transpose()?,
                resume,
                max_lookback,
            },
            CoreSubscription::Cron { schedule } => Self::Cron { schedule },
        })
    }
}

impl Subscription {
    /// The kind dispatch: a core kind must match its shape, an unknown
    /// kind becomes [`Subscription::Extension`] with string filters.
    /// `index` is the table's 1-based position; a refusal carries it
    /// because the parsed tables have no source spans.
    fn from_table(index: usize, table: toml::Table) -> Result<Self, ParseError> {
        let Some(kind) = table.get("kind").and_then(toml::Value::as_str) else {
            return Err(ParseError::MissingSubscriptionKind { index });
        };
        if kind.parse::<nexum_world::SubscriptionKind>().is_err() {
            let kind = kind.to_owned();
            let mut filters = BTreeMap::new();
            for (key, value) in table {
                if key == "kind" {
                    continue;
                }
                let toml::Value::String(value) = value else {
                    return Err(ParseError::NonStringSubscriptionFilter { key });
                };
                filters.insert(key, value);
            }
            return Ok(Self::Extension { kind, filters });
        }
        let kind = kind.to_owned();
        toml::Value::Table(table)
            .try_into::<CoreSubscription>()
            .map_err(|source| ParseError::InvalidSubscription {
                index,
                kind,
                source,
            })?
            .try_into()
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentSection {
    /// Instance identity; keys the keccak local-store namespace, so it is
    /// unique across `[[modules]]`.
    #[serde(default)]
    pub name: String,
    #[allow(dead_code)] // Parsed but has no reader.
    #[serde(default)]
    pub version: String,
    /// Pinned `sha256:<64 hex chars>` digest, verified against the loaded
    /// bytes before compile.
    #[serde(default)]
    pub digest: Option<String>,
    /// Per-component resource requests; each unset field inherits the
    /// component's `[policy]` ceiling and never widens it.
    #[serde(default)]
    pub resources: ResourceSection,
}

/// `[component.resources]` overrides; each unset field keeps the
/// component's `[policy]` ceiling.
///
/// A set field narrows and never widens: this manifest is author-supplied,
/// so the `[policy]` value is a ceiling. A field above it is capped.
/// `deny_unknown_fields` so a misspelled key refuses rather than silently
/// dropping the ceiling the author asked for.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResourceSection {
    /// Linear-memory cap, in bytes.
    #[serde(default)]
    pub max_memory_bytes: Option<usize>,
    /// Fuel granted per dispatch.
    #[serde(default)]
    pub max_fuel_per_dispatch: Option<u64>,
    /// Local-store byte quota (key + value bytes).
    #[serde(default)]
    pub max_state_bytes: Option<u64>,
}

/// `[dependencies]`: each key names a host capability or a service, and
/// its table carries the attributes that qualify it.
pub type DependencySection = BTreeMap<String, Dependency>;

/// One dependency's attributes. `deny_unknown_fields` so a misspelled
/// attribute refuses rather than being ignored.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    /// Hosts this component may reach. Only the `http` dependency takes
    /// it; load refuses it anywhere else.
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Validated manifest: every value check has run, so each field carries
/// its typed form.
#[derive(Debug)]
pub struct LoadedManifest {
    /// `[component].name` parsed into the namespace.
    pub name: crate::module_id::ModuleId,
    /// `[component].digest` parsed to its typed digest.
    pub component_digest: Option<crate::digest::ContentDigest>,
    /// `[component.resources]` overrides.
    pub resources: ResourceSection,
    /// `[dependencies]`; presence is validated, an absent table refuses.
    pub dependencies: DependencySection,
    /// Hosts wasi:http outgoing requests may target, each parsed from an
    /// exact hostname or a `*.suffix` wildcard entry.
    pub http_allowlist: Vec<HostPattern>,
    /// `[config]` flattened to `(key, stringified-value)` pairs for a
    /// module's `init`. Scalars become their text form; arrays and tables
    /// their TOML representation.
    pub config: Vec<(String, String)>,
    /// Parsed `[[subscription]]` tables.
    pub subscriptions: Vec<Subscription>,
    /// Extension-owned sections.
    pub extensions: ExtensionSections,
}

impl TryFrom<Manifest> for LoadedManifest {
    type Error = ParseError;

    /// Every context-free value check, in order: name, digest,
    /// subscriptions, then `[dependencies]` presence. The registry
    /// cross-check and the `hosts` placement check stay in `load`, which
    /// holds the registry and refuses an unknown name first.
    fn try_from(manifest: Manifest) -> Result<Self, ParseError> {
        // The only producer of a `ModuleId`.
        let name = crate::module_id::ModuleId::parse(&manifest.component.name)?;
        let component_digest = manifest
            .component
            .digest
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|source| ParseError::InvalidComponentDigest {
                value: manifest.component.digest.clone().unwrap_or_default(),
                source,
            })?;
        let subscriptions = manifest
            .subscriptions
            .into_iter()
            .zip(1..)
            .map(|(table, index)| Subscription::from_table(index, table))
            .collect::<Result<Vec<_>, _>>()?;
        let dependencies = manifest
            .dependencies
            .ok_or(ParseError::MissingCapabilities)?;
        let http_allowlist = dependencies
            .get(nexum_world::Cap::Http.as_str())
            .map(|dep| {
                dep.hosts
                    .iter()
                    .map(|h| HostPattern::from(h.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let config = manifest
            .config
            .iter()
            .map(|(k, v)| (k.clone(), stringify_toml_value(v)))
            .collect();
        Ok(Self {
            name,
            component_digest,
            resources: manifest.component.resources,
            dependencies,
            http_allowlist,
            config,
            subscriptions,
            extensions: manifest.extensions,
        })
    }
}

fn stringify_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(_) | toml::Value::Table(_) => v.to_string(),
    }
}
