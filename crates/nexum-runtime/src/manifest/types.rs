//! Serde shapes: `Manifest`, its sections, and `LoadedManifest`.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256};
use serde::Deserialize;
use serde::de::Error as _;

/// Core capability names: the `nexum:host` interfaces linked into every
/// module. `http` is gated separately (it gates `wasi:http/*`), and
/// extensions register their own namespaces.
pub const CORE_CAPABILITIES: &[&str] = &nexum_world::CORE_IFACES;

#[derive(Debug, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub module: ModuleSection,
    #[serde(default)]
    pub capabilities: Option<CapabilitiesSection>,
    #[serde(default)]
    pub config: toml::Table,
    /// Event subscriptions wired before `_init`. `block` and `chain-log`
    /// are dispatched; `cron` is parsed and ignored.
    #[serde(default, rename = "subscription")]
    pub subscriptions: Vec<Subscription>,
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

/// Core subscription kinds parsed by shape; others fall through to
/// [`Subscription::Extension`].
// `kebab-case` reproduces `nexum_world::SubscriptionKind`, which gates this.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum CoreSubscription {
    Block {
        chain_id: u64,
    },
    ChainLog {
        chain_id: u64,
        #[serde(default, deserialize_with = "chain_log_address")]
        address: Option<Address>,
        #[serde(default, deserialize_with = "chain_log_topic")]
        event_signature: Option<B256>,
        #[serde(default)]
        resume: bool,
        #[serde(default)]
        max_lookback: Option<u64>,
    },
    Cron {
        schedule: String,
    },
}

fn chain_log_address<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Address>, D::Error> {
    // Pinned operator wording.
    hex_field(d, "invalid chain-log address")
}

fn chain_log_topic<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<B256>, D::Error> {
    // Pinned operator wording.
    hex_field(d, "invalid topic")
}

/// Refusal lands at manifest load; `label` carries the pinned wording.
fn hex_field<'de, D, T>(d: D, label: &str) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = String::deserialize(d)?;
    raw.parse()
        .map(Some)
        .map_err(|e| D::Error::custom(format!("{label} {raw:?}: {e}")))
}

impl From<CoreSubscription> for Subscription {
    fn from(sub: CoreSubscription) -> Self {
        match sub {
            CoreSubscription::Block { chain_id } => Self::Block { chain_id },
            CoreSubscription::ChainLog {
                chain_id,
                address,
                event_signature,
                resume,
                max_lookback,
            } => Self::ChainLog {
                chain_id,
                address,
                event_signature,
                resume,
                max_lookback,
            },
            CoreSubscription::Cron { schedule } => Self::Cron { schedule },
        }
    }
}

impl<'de> Deserialize<'de> for Subscription {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let Some(kind) = table.get("kind").and_then(toml::Value::as_str) else {
            return Err(D::Error::missing_field("kind"));
        };
        match kind.parse::<nexum_world::SubscriptionKind>() {
            Ok(_) => toml::Value::Table(table.clone())
                .try_into::<CoreSubscription>()
                .map(Into::into)
                .map_err(D::Error::custom),
            Err(_) => {
                let kind = kind.to_owned();
                let mut filters = BTreeMap::new();
                for (key, value) in table {
                    if key == "kind" {
                        continue;
                    }
                    let Some(value) = value.as_str() else {
                        return Err(D::Error::custom(format!(
                            "subscription filter `{key}` must be a string"
                        )));
                    };
                    filters.insert(key, value.to_owned());
                }
                Ok(Self::Extension { kind, filters })
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ModuleSection {
    #[serde(default)]
    pub name: String,
    #[allow(dead_code)] // Parsed but has no reader.
    #[serde(default)]
    pub version: String,
    /// Pinned `sha256:<64 hex chars>` digest, verified against the loaded
    /// bytes before compile.
    #[serde(default)]
    pub component: Option<String>,
    /// Component kind; defaults to the worker (`event-module`), a provider
    /// names its registered kind.
    #[serde(default)]
    pub kind: ComponentKind,
    /// Per-module resource overrides; each unset field inherits the engine
    /// `[limits]` default.
    #[serde(default)]
    pub resources: ResourceSection,
}

/// The worker kind's manifest spelling.
pub const WORKER_KIND: &str = "event-module";

/// Component kind a manifest declares: the worker, or a provider spelling
/// an extension registers. Defaults to the worker; an unregistered spelling
/// is refused at boot.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq, derive_more::Display)]
#[serde(from = "String")]
pub enum ComponentKind {
    /// Event-driven worker (`event-module`).
    #[default]
    #[display("{WORKER_KIND}")]
    Worker,
    /// A provider, named by its manifest spelling.
    #[display("{_0}")]
    Provider(String),
}

impl From<String> for ComponentKind {
    fn from(kind: String) -> Self {
        if kind == WORKER_KIND {
            Self::Worker
        } else {
            Self::Provider(kind)
        }
    }
}

/// `[module.resources]` overrides; each unset field keeps the engine
/// `[limits]` default.
#[derive(Debug, Deserialize, Default)]
pub struct ResourceSection {
    /// Linear-memory cap, in bytes.
    #[serde(default)]
    pub max_memory_bytes: Option<usize>,
    /// Fuel granted per event dispatch.
    #[serde(default)]
    pub max_fuel_per_event: Option<u64>,
    /// Local-store byte quota (key + value bytes).
    #[serde(default)]
    pub max_state_bytes: Option<u64>,
}

/// `deny_unknown_fields` so a manifest still carrying the removed
/// `optional` key refuses at parse rather than losing the declaration.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesSection {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub http: Option<HttpSection>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HttpSection {
    #[serde(default)]
    pub allow: Vec<String>,
}

/// Loaded + validated manifest, plus the data the engine needs to
/// instantiate a module.
#[derive(Debug)]
pub struct LoadedManifest {
    pub manifest: Manifest,
    /// Hosts wasi:http outgoing requests may target. Each entry is
    /// either an exact hostname or a `*.suffix` wildcard.
    pub http_allowlist: Vec<String>,
    /// `[config]` flattened to `(key, stringified-value)` pairs for a
    /// module's `init`. Scalars become their text form; arrays and tables
    /// their TOML representation.
    pub config: Vec<(String, String)>,
    /// `[module].component` parsed to its typed digest.
    pub component_digest: Option<crate::digest::ContentDigest>,
}
