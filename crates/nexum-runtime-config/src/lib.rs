//! Engine-side runtime configuration (`engine.toml`).

// `with_env` in the `load` tests calls `std::env::set_var`, which is
// unsafe as of the 2024 edition.
#![cfg_attr(not(test), forbid(unsafe_code))]

mod chain;
mod dispatch_rate;
mod error;
mod limits;
mod load;
mod poison_policy;
mod policy;

pub use chain::{ChainConfig, RpcEndpoint, RpcEndpointError, RpcTransport};
pub use dispatch_rate::{
    DEFAULT_DISPATCH_BURST, DEFAULT_DISPATCH_REFILL_PER_SEC, DispatchRatePolicy, TokenBucket,
};
pub use error::{EngineConfigError, EnvVarError};
pub use limits::{
    ChainLimitsSection, DispatchLimitsSection, HttpLimitsSection, LogLimitsSection,
    LogRetentionLimits, ModuleLimits, OutboundHttpLimits, PoisonLimitsSection,
    ResolvedModuleLimits, ShutdownLimitsSection,
};
pub use load::load_or_default;
pub use poison_policy::{POISON_MAX_FAILURES, POISON_WINDOW, PoisonPolicy, should_poison};
pub use policy::{
    ComponentPolicy, EffectivePolicy, LogBoundsPolicy, LogFilterPolicy, LogVerdict, PolicyCeilings,
    PolicySection, TotalPolicy,
};

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use alloy_chains::Chain;
use serde::Deserialize;

use chain::{RawChainConfig, resolve_chains};
use nexum_primitives::digest::ContentDigest;
use policy::{RawPolicySection, resolve_policy};

/// A literal as non-zero; a zero fails the build.
const fn nz_usize(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(v) => v,
        None => panic!("zero constant"),
    }
}

/// As [`nz_usize`], for `u64` constants.
const fn nz_u64(n: u64) -> NonZeroU64 {
    match NonZeroU64::new(n) {
        Some(v) => v,
        None => panic!("zero constant"),
    }
}

/// As [`nz_usize`], for `u32` constants.
const fn nz_u32(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(v) => v,
        None => panic!("zero constant"),
    }
}

/// Engine-side configuration loaded from `engine.toml`. Deserialization
/// goes through a raw shape whose `TryFrom` conversion validates the
/// `[chains]` keys, so this type never carries an unvalidated key.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "RawEngineConfig")]
#[non_exhaustive]
pub struct EngineConfig {
    /// Process-wide settings: state directory, log level, metrics.
    pub engine: EngineSection,
    /// Per-module limits other than the `[policy]` ceilings.
    pub limits: ResolvedModuleLimits,
    /// Operator resource and egress policy.
    pub policy: PolicySection,
    /// Per-chain RPC config keyed by EIP-155 chain id. Numeric
    /// (`[chains.11155111]`) and named (`[chains.sepolia]`) keys both
    /// validate via `Chain`'s `FromStr` after the TOML parse.
    pub chains: HashMap<Chain, ChainConfig>,
    /// Opaque `[extensions.<name>]` tables; the engine never interprets
    /// these, each extension parses its own at the composition root.
    pub extensions: HashMap<String, toml::Value>,
    /// Modules the supervisor boots; each resolves a
    /// `(component.wasm, component.toml)` pair.
    pub modules: Vec<ModuleEntry>,
    /// True when [`load_or_default`] found no engine.toml.
    pub defaulted: bool,
}

/// Raw deserialized engine config; the `[chains]` keys stay as written
/// until the `TryFrom` conversion into [`EngineConfig`] validates them.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngineConfig {
    #[serde(default)]
    engine: EngineSection,
    #[serde(default)]
    limits: ModuleLimits,
    #[serde(default)]
    policy: RawPolicySection,
    #[serde(default)]
    chains: HashMap<String, RawChainConfig>,
    #[serde(default)]
    extensions: HashMap<String, toml::Value>,
    #[serde(default)]
    modules: Vec<RawModuleEntry>,
}

impl TryFrom<RawEngineConfig> for EngineConfig {
    type Error = EngineConfigError;

    /// The value checks serde defers, as typed refusals rather than serde
    /// strings. The derived `Deserialize` runs this too, so the public
    /// `toml::from_str` path cannot yield an unvalidated config.
    fn try_from(raw: RawEngineConfig) -> Result<Self, EngineConfigError> {
        let chains = resolve_chains(raw.chains)?;
        // The ids are the `[policy.component]` join column: non-empty and
        // unique, checked before the policy rows that key on them.
        let mut ids = HashSet::with_capacity(raw.modules.len());
        for entry in &raw.modules {
            if entry.id.trim().is_empty() {
                return Err(EngineConfigError::EmptyComponentId {
                    path: entry.path.clone(),
                });
            }
            if !ids.insert(entry.id.as_str()) {
                return Err(EngineConfigError::DuplicateComponentId {
                    id: entry.id.clone(),
                });
            }
        }
        let policy = resolve_policy(raw.policy, &ids)?;
        let modules = raw
            .modules
            .into_iter()
            .map(ModuleEntry::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            engine: raw.engine,
            limits: raw.limits.try_into()?,
            policy,
            chains,
            extensions: raw.extensions,
            modules,
            defaulted: false,
        })
    }
}

/// Raw `[[modules]]` table; the digest stays as written until the
/// [`ModuleEntry`] conversion validates it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModuleEntry {
    id: String,
    path: std::path::PathBuf,
    #[serde(default)]
    manifest: Option<std::path::PathBuf>,
    #[serde(default)]
    digest: Option<String>,
}

/// One `[[modules]]` table. `manifest` defaults to a sibling
/// `component.toml`. Deserialization goes through the raw shape, so a
/// standalone parse validates the digest exactly as the config load does.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawModuleEntry")]
#[non_exhaustive]
pub struct ModuleEntry {
    /// Operator-written identity; the `[policy.component.<id>]` key. The
    /// author-supplied `[component].name` never binds policy (ADR-0001).
    pub id: String,
    /// Path to the compiled `.wasm` component.
    pub path: std::path::PathBuf,
    /// Path to the module's `component.toml`. Defaults to `<path-parent>/component.toml`.
    pub manifest: Option<std::path::PathBuf>,
    /// The operator's pin on this entry's artifact, verified against the
    /// exact bytes handed to the compiler. Independent of the author's
    /// `[component].digest`: both are verified when present.
    pub digest: Option<ContentDigest>,
}

impl ModuleEntry {
    /// Leaves the manifest to sibling discovery and the artifact unpinned.
    pub fn new(id: impl Into<String>, path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            manifest: None,
            digest: None,
        }
    }
}

impl TryFrom<RawModuleEntry> for ModuleEntry {
    type Error = EngineConfigError;

    fn try_from(raw: RawModuleEntry) -> Result<Self, EngineConfigError> {
        let digest = match raw.digest {
            Some(value) => Some(value.parse::<ContentDigest>().map_err(|source| {
                EngineConfigError::InvalidModuleDigest {
                    id: raw.id.clone(),
                    value,
                    source,
                }
            })?),
            None => None,
        };
        Ok(Self {
            id: raw.id,
            path: raw.path,
            manifest: raw.manifest,
            digest,
        })
    }
}

/// `[engine]`: settings that apply to the process, not to one module.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct EngineSection {
    /// Root of the on-disk state. Each module gets a namespace under it.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// `EnvFilter` directive; defaults to `info`, `RUST_LOG` overrides at
    /// process start.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Prometheus exporter wiring. Absent = disabled (the recorder is still
    /// installed so call sites stay live, but no HTTP listener binds).
    #[serde(default)]
    pub metrics: MetricsSection,
    /// Per-block `eth_getLogs` concurrency during chain-log backfill. `0` is
    /// treated as `1`.
    #[serde(default = "default_log_backfill_concurrency")]
    pub log_backfill_concurrency: usize,
    /// Refuse to boot any component without a `[component].digest` pin; a
    /// present pin is verified regardless.
    #[serde(default)]
    pub require_component_digest: bool,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            log_level: default_log_level(),
            metrics: MetricsSection::default(),
            log_backfill_concurrency: default_log_backfill_concurrency(),
            require_component_digest: false,
        }
    }
}

fn default_log_backfill_concurrency() -> usize {
    16
}

/// `[engine.metrics]`. When `enabled`, serves `/metrics` on `bind_addr`
/// via a Prometheus HTTP exporter. Default disabled.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetricsSection {
    /// Bind the HTTP listener. False still installs the recorder.
    #[serde(default)]
    pub enabled: bool,
    /// IPv4 / IPv6 socket address to bind. Default `127.0.0.1:9100`.
    #[serde(default = "default_metrics_bind")]
    pub bind_addr: String,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: default_metrics_bind(),
        }
    }
}

fn default_metrics_bind() -> String {
    "127.0.0.1:9100".to_owned()
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_log_level() -> String {
    "info".to_owned()
}

// The dual-path tests here pin the serde(try_from) seam: each refusal
// asserts both the typed TryFrom error and the public toml::from_str
// path. They must not migrate into the section files or lose their
// public-parse halves.
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn named_chain_key_round_trips_to_the_chain() {
        // A named TOML key must validate to the same `Chain` the numeric
        // id would, because the conversion forwards the key string to
        // `Chain`'s `FromStr`.
        let cfg = toml::from_str::<EngineConfig>(
            r#"
[chains.sepolia]
rpc_url = "wss://example.test/sepolia"
"#,
        )
        .expect("named chain key parses");
        assert!(
            cfg.chains.contains_key(&Chain::sepolia()),
            "the [chains.sepolia] table keys on the Sepolia chain",
        );
        let endpoint = &cfg
            .chains
            .get(&Chain::sepolia())
            .expect("sepolia entry")
            .rpc_url;
        assert_eq!(endpoint.url().as_str(), "wss://example.test/sepolia");
        assert!(
            endpoint.supports_pubsub(),
            "wss selects the pubsub transport"
        );
        assert_eq!(endpoint.transport(), RpcTransport::WebSocket);
    }

    #[test]
    fn a_credentialed_endpoint_keeps_full_fidelity_host_side() {
        // The secrecy boundary is the WIT edge, not the operator log:
        // host-side `Debug` and the dial path carry the URL as configured.
        const URL: &str = "https://user:hunter2secret@rpc.example.com\
             /AnOfyGnZ0nWpSOOwQzqAnFjNaa0sR8ZxkVjewFaCJ?apikey=querysecret#fragsecret";
        let cfg = toml::from_str::<EngineConfig>(&format!("[chains.1]\nrpc_url = \"{URL}\"\n"))
            .expect("credentialed URL parses");
        let endpoint = &cfg.chains.get(&Chain::from_id(1)).expect("entry").rpc_url;
        assert_eq!(endpoint.url().as_str(), URL);
        assert!(
            format!("{cfg:?}").contains("hunter2secret"),
            "Debug keeps the credential for host diagnostics",
        );
    }

    #[test]
    fn a_malformed_rpc_url_refuses_at_load_with_a_typed_error() {
        const BAD: &str = "[chains.1]\nrpc_url = \"not a url\"\n";
        let raw = toml::from_str::<RawEngineConfig>(BAD)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("a malformed URL must not validate");
        assert!(
            matches!(
                err,
                EngineConfigError::InvalidRpcUrl {
                    ref key,
                    source: RpcEndpointError::Parse(_),
                } if key == "1"
            ),
            "{err:?}",
        );
        let err = toml::from_str::<EngineConfig>(BAD).expect_err("a malformed URL must not parse");
        assert!(err.to_string().contains("chains.1.rpc_url"), "{err}");
    }

    #[test]
    fn an_unsupported_rpc_scheme_refuses_at_load() {
        const FTP: &str = "[chains.1]\nrpc_url = \"ftp://rpc.example.com/x\"\n";
        let raw = toml::from_str::<RawEngineConfig>(FTP)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("an ftp URL must not validate");
        assert!(
            matches!(
                err,
                EngineConfigError::InvalidRpcUrl {
                    ref key,
                    source: RpcEndpointError::UnsupportedScheme { ref scheme },
                } if key == "1" && scheme == "ftp"
            ),
            "{err:?}",
        );
    }

    #[test]
    fn zero_request_timeout_is_rejected_at_load() {
        const ZERO: &str = r#"
[chains.1]
rpc_url = "http://example.test/x"
request_timeout_secs = 0
"#;
        let raw = toml::from_str::<RawEngineConfig>(ZERO)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("a zero timeout must not validate");
        assert!(
            matches!(err, EngineConfigError::ZeroField { ref field }
                if field == "chains.1.request_timeout_secs"),
            "{err:?}",
        );
        // The public `Deserialize` path funnels through the same
        // conversion; pins the operator-facing message.
        let err = toml::from_str::<EngineConfig>(ZERO).expect_err("a zero timeout must not parse");
        assert!(
            err.to_string()
                .contains("request_timeout_secs must not be 0"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn max_log_range_blocks_defaults_and_overrides() {
        let cfg = toml::from_str::<EngineConfig>("[chains.1]\nrpc_url = \"http://example.test\"\n")
            .expect("entry without the key parses");
        assert_eq!(
            cfg.chains
                .get(&Chain::from_id(1))
                .expect("entry")
                .max_log_range_blocks,
            1000,
        );
        let cfg = toml::from_str::<EngineConfig>(
            "[chains.1]\nrpc_url = \"http://example.test\"\nmax_log_range_blocks = 5000\n",
        )
        .expect("the override parses");
        assert_eq!(
            cfg.chains
                .get(&Chain::from_id(1))
                .expect("entry")
                .max_log_range_blocks,
            5000,
        );
    }

    #[test]
    fn zero_max_log_range_blocks_is_rejected_at_load() {
        const ZERO: &str = r#"
[chains.1]
rpc_url = "http://example.test/x"
max_log_range_blocks = 0
"#;
        let raw = toml::from_str::<RawEngineConfig>(ZERO)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("a zero range must not validate");
        assert!(
            matches!(err, EngineConfigError::ZeroField { ref field }
                if field == "chains.1.max_log_range_blocks"),
            "{err:?}",
        );
        let err = toml::from_str::<EngineConfig>(ZERO).expect_err("a zero range must not parse");
        assert!(
            err.to_string()
                .contains("max_log_range_blocks must not be 0"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn invalid_chain_key_surfaces_a_typed_refusal() {
        // A key that is neither a numeric id nor a known chain name must
        // fail validation with a variant carrying the key, not silently
        // drop and not hide inside a serde string.
        const BOGUS: &str = "[chains.bogus]\nrpc_url = \"wss://example.test/x\"\n";
        let raw = toml::from_str::<RawEngineConfig>(BOGUS)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("bogus chain key must not validate");
        assert!(
            matches!(err, EngineConfigError::InvalidChainKey { ref key } if key == "bogus"),
            "{err:?}",
        );
        // The public `Deserialize` path runs the same conversion, so the
        // refusal survives (as a serde string) and nothing drops silently.
        let err = toml::from_str::<EngineConfig>(BOGUS)
            .expect_err("the derived Deserialize must refuse too");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn require_component_digest_defaults_false_and_parses() {
        assert!(!EngineConfig::default().engine.require_component_digest);
        let cfg: EngineConfig = toml::from_str("[engine]\nrequire_component_digest = true\n")
            .expect("the [engine] flag parses");
        assert!(cfg.engine.require_component_digest);
    }

    /// An ignored key reads as an absent one, and an absent policy section
    /// is the permissive case, so a typo must refuse rather than parse.
    #[test]
    fn an_unknown_key_refuses_and_names_itself() {
        for (label, toml) in [
            ("top-level section", "[polcy]\nmax_memory_bytes = 1\n"),
            ("key in a section", "[engine]\nstate_dr = \"./data\"\n"),
            (
                "key in a nested section",
                "[limits.http]\ntotal_deadline_ms = 1\nresponse_body_max_byte = 1\n",
            ),
            (
                "key in a table entry",
                "[[modules]]\nid = \"m\"\npath = \"m.wasm\"\nmanifets = \"c.toml\"\n",
            ),
            (
                "key in a policy row",
                "[[modules]]\nid = \"m\"\npath = \"m.wasm\"\n[policy.component.m]\nmax_memory_byte = 1\n",
            ),
            (
                "retired [limits.watch] section",
                "[limits.watch]\nmax_entries = 1\n",
            ),
            (
                "retired [limits.quota] section",
                "[limits.quota]\nmax_charges = 1\n",
            ),
            (
                "retired [limits] deadline key",
                "[limits]\nevent_deadline_secs = 30\n",
            ),
            (
                // ADR-0022 cut `[implements]`; a stale table refuses at
                // parse as an unknown key rather than binding nothing.
                "retired [implements] table",
                "[[modules]]\nid = \"m\"\npath = \"m.wasm\"\n\
                 [implements.\"a:b/c@1\"]\ncomponent = \"m\"\n",
            ),
        ] {
            let err = toml::from_str::<EngineConfig>(toml)
                .expect_err(&format!("{label} must refuse an unknown key"));
            let msg = err.to_string();
            assert!(msg.contains("unknown"), "{label}: {msg}");
        }
    }

    /// The guard must not reject what the schema does accept.
    #[test]
    fn a_fully_populated_config_still_parses() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[engine]
state_dir = "./data"

[limits.dispatch]
deadline_secs = 30

[limits.http]
total_deadline_ms = 1000

[policy]
max_memory_bytes      = 268435456
max_fuel_per_dispatch = 7
max_state_bytes       = 1024
capabilities       = ["chain", "logging", "http"]
http_deny          = ["169.254.0.0/16"]

[policy.total]
max_memory_bytes = 4294967296

[policy.component.m]
max_memory_bytes = 1073741824
http_allow       = ["api.cow.fi"]

[chains.1]
rpc_url = "https://example.test"

[extensions.acme]
anything = "goes here, the engine never reads it"

[[modules]]
id = "m"
path = "m.wasm"
digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
"#,
        )
        .expect("every documented section parses under the guard");
        assert_eq!(cfg.policy.ceilings.max_fuel_per_dispatch.get(), 7);
        assert_eq!(cfg.modules.len(), 1);
        assert_eq!(cfg.modules[0].id, "m");
        assert!(cfg.modules[0].digest.is_some());
        assert!(
            cfg.extensions.contains_key("acme"),
            "an extension table stays opaque and unguarded",
        );
    }

    /// The install path for `[[services]]` is gone, so the table refuses at
    /// parse rather than surviving as an entry nothing loads.
    #[test]
    fn a_services_table_refuses_at_parse() {
        let err = toml::from_str::<EngineConfig>("[[services]]\npath = \"s.wasm\"\n")
            .expect_err("a [[services]] table must refuse");
        let msg = err.to_string();
        assert!(msg.contains("unknown") && msg.contains("services"), "{msg}");
    }

    /// Pins the built-in numbers, not the constants, so a resolution that
    /// pairs a field with the wrong default constant fails here. The parse
    /// path fills its own fallbacks in `resolve_policy`, so it is pinned
    /// separately from the `Default` impl.
    #[test]
    fn core_limits_default_when_absent() {
        let limits = ResolvedModuleLimits::default();
        assert_eq!(limits.dispatch_deadline, Duration::from_secs(120));
        let parsed: EngineConfig = toml::from_str("").expect("empty config parses");
        for ceilings in [PolicyCeilings::default(), parsed.policy.ceilings] {
            assert_eq!(ceilings.max_fuel_per_dispatch.get(), 1_000_000_000);
            assert_eq!(ceilings.max_memory_bytes.get(), 64 * 1024 * 1024);
            assert_eq!(ceilings.max_state_bytes, 50 * 1024 * 1024);
        }
    }

    #[test]
    fn core_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.dispatch]
deadline_secs = 30

[policy]
max_fuel_per_dispatch = 7
max_memory_bytes      = 1_048_576
max_state_bytes       = 2_048
"#,
        )
        .expect("top-level limits parse");
        assert_eq!(cfg.limits.dispatch_deadline, Duration::from_secs(30));
        assert_eq!(cfg.policy.ceilings.max_fuel_per_dispatch.get(), 7);
        assert_eq!(cfg.policy.ceilings.max_memory_bytes.get(), 1_048_576);
        assert_eq!(cfg.policy.ceilings.max_state_bytes, 2_048);
    }

    #[test]
    fn a_modules_entry_without_an_id_refuses_at_parse() {
        let err = toml::from_str::<EngineConfig>("[[modules]]\npath = \"m.wasm\"\n")
            .expect_err("id is required");
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[test]
    fn a_blank_or_duplicate_module_id_refuses() {
        let raw = toml::from_str::<RawEngineConfig>("[[modules]]\nid = \" \"\npath = \"m.wasm\"\n")
            .expect("raw parse");
        let err = EngineConfig::try_from(raw).expect_err("a blank id must not validate");
        assert!(
            matches!(err, EngineConfigError::EmptyComponentId { .. }),
            "{err:?}"
        );

        let raw = toml::from_str::<RawEngineConfig>(
            "[[modules]]\nid = \"m\"\npath = \"a.wasm\"\n[[modules]]\nid = \"m\"\npath = \"b.wasm\"\n",
        )
        .expect("raw parse");
        let err = EngineConfig::try_from(raw).expect_err("a duplicate id must not validate");
        assert!(
            matches!(err, EngineConfigError::DuplicateComponentId { ref id } if id == "m"),
            "{err:?}",
        );
    }

    #[test]
    fn a_modules_digest_parses_to_a_typed_pin() {
        let pin = format!("sha256:{}", "ab".repeat(32));
        let cfg: EngineConfig = toml::from_str(&format!(
            "[[modules]]\nid = \"m\"\npath = \"m.wasm\"\ndigest = \"{pin}\"\n\
             [[modules]]\nid = \"n\"\npath = \"n.wasm\"\n",
        ))
        .expect("[[modules]] digests parse");
        let pinned = &cfg.modules[0];
        assert_eq!(pinned.digest.expect("pin parsed").to_string(), pin);
        assert!(
            cfg.modules[1].digest.is_none(),
            "an absent operator pin is the permitted default",
        );
    }

    /// `ModuleEntry` is public API a downstream config can embed, so the
    /// standalone `Deserialize` impl must survive the raw-shape split,
    /// and it must validate the digest exactly as the config load does.
    #[test]
    fn a_module_entry_deserializes_standalone() {
        let pin = format!("sha256:{}", "ab".repeat(32));
        let entry: ModuleEntry = toml::from_str(&format!(
            "id = \"m\"\npath = \"m.wasm\"\ndigest = \"{pin}\"\n"
        ))
        .expect("a standalone entry parses");
        assert_eq!(entry.id, "m");
        assert_eq!(entry.digest.expect("pin parsed").to_string(), pin);
        let err =
            toml::from_str::<ModuleEntry>("id = \"m\"\npath = \"m.wasm\"\ndigest = \"bad\"\n")
                .expect_err("a malformed pin must refuse standalone too");
        assert!(err.to_string().contains("bad"), "{err}");
    }

    #[test]
    fn a_malformed_modules_digest_refuses() {
        const BAD: &str = "[[modules]]\nid = \"m\"\npath = \"m.wasm\"\ndigest = \"notahash\"\n";
        let raw = toml::from_str::<RawEngineConfig>(BAD)
            .expect("the raw parse only decides the TOML is well formed");
        let err = EngineConfig::try_from(raw).expect_err("a malformed pin must not validate");
        assert!(
            matches!(err, EngineConfigError::InvalidModuleDigest { ref id, ref value, .. }
                if id == "m" && value == "notahash"),
            "{err:?}",
        );
        let err = toml::from_str::<EngineConfig>(BAD)
            .expect_err("the derived Deserialize must refuse too");
        assert!(err.to_string().contains("notahash"), "{err}");
    }

    #[test]
    fn a_policy_row_matching_no_module_id_refuses() {
        let raw = toml::from_str::<RawEngineConfig>(
            "[policy.component.wallet]\nmax_memory_bytes = 1\n\
             [[modules]]\nid = \"tracker\"\npath = \"t.wasm\"\n",
        )
        .expect("raw parse");
        let err = EngineConfig::try_from(raw).expect_err("a dangling policy row must not validate");
        assert!(
            matches!(err, EngineConfigError::UnknownPolicyComponent { ref id } if id == "wallet"),
            "{err:?}",
        );
    }

    /// Every `[limits]` zero that used to saturate silently now refuses at
    /// load, through the typed conversion and the public parse alike.
    #[test]
    fn a_zero_limit_refuses_at_load_naming_the_field() {
        for (toml, field) in [
            (
                "[limits.dispatch]\ndeadline_secs = 0\n",
                "limits.dispatch.deadline_secs",
            ),
            (
                "[policy]\nmax_fuel_per_dispatch = 0\n",
                "policy.max_fuel_per_dispatch",
            ),
            (
                "[policy]\nmax_memory_bytes = 0\n",
                "policy.max_memory_bytes",
            ),
            (
                "[policy.total]\nmax_memory_bytes = 0\n",
                "policy.total.max_memory_bytes",
            ),
            (
                "[policy.component.m]\nmax_memory_bytes = 0\n\
                 [[modules]]\nid = \"m\"\npath = \"m.wasm\"\n",
                "policy.component.m.max_memory_bytes",
            ),
            (
                "[policy.component.m]\nmax_fuel_per_dispatch = 0\n\
                 [[modules]]\nid = \"m\"\npath = \"m.wasm\"\n",
                "policy.component.m.max_fuel_per_dispatch",
            ),
            (
                "[policy]\nmax_log_record_bytes = 0\n",
                "policy.max_log_record_bytes",
            ),
            ("[policy]\nmax_log_burst = 0\n", "policy.max_log_burst"),
            (
                "[policy]\nmax_log_records_per_sec = 0\n",
                "policy.max_log_records_per_sec",
            ),
            (
                "[policy.component.m]\nmax_log_record_bytes = 0\n\
                 [[modules]]\nid = \"m\"\npath = \"m.wasm\"\n",
                "policy.component.m.max_log_record_bytes",
            ),
            (
                "[policy.component.m]\nmax_log_burst = 0\n\
                 [[modules]]\nid = \"m\"\npath = \"m.wasm\"\n",
                "policy.component.m.max_log_burst",
            ),
            (
                "[policy.component.m]\nmax_log_records_per_sec = 0\n\
                 [[modules]]\nid = \"m\"\npath = \"m.wasm\"\n",
                "policy.component.m.max_log_records_per_sec",
            ),
            (
                "[limits.chain]\nresponse_body_max_bytes = 0\n",
                "limits.chain.response_body_max_bytes",
            ),
            (
                "[limits.http]\nconnect_timeout_max_ms = 0\n",
                "limits.http.connect_timeout_max_ms",
            ),
            (
                "[limits.http]\nfirst_byte_timeout_max_ms = 0\n",
                "limits.http.first_byte_timeout_max_ms",
            ),
            (
                "[limits.http]\nbetween_bytes_timeout_max_ms = 0\n",
                "limits.http.between_bytes_timeout_max_ms",
            ),
            (
                "[limits.http]\ntotal_deadline_ms = 0\n",
                "limits.http.total_deadline_ms",
            ),
            (
                "[limits.logs]\nbytes_per_run = 0\n",
                "limits.logs.bytes_per_run",
            ),
            (
                "[limits.logs]\nruns_retained = 0\n",
                "limits.logs.runs_retained",
            ),
            (
                "[limits.poison]\nmax_failures = 0\n",
                "limits.poison.max_failures",
            ),
            (
                "[limits.poison]\nwindow_secs = 0\n",
                "limits.poison.window_secs",
            ),
            ("[limits.dispatch]\nburst = 0\n", "limits.dispatch.burst"),
            (
                "[limits.dispatch]\nrefill_per_sec = 0\n",
                "limits.dispatch.refill_per_sec",
            ),
        ] {
            let raw = toml::from_str::<RawEngineConfig>(toml)
                .expect("the raw parse only decides the TOML is well formed");
            let err =
                EngineConfig::try_from(raw).expect_err(&format!("{field} = 0 must not validate"));
            assert!(
                matches!(err, EngineConfigError::ZeroField { field: ref f } if f == field),
                "{field}: {err:?}",
            );
            // The public `Deserialize` path refuses too, naming the field.
            let err = toml::from_str::<EngineConfig>(toml)
                .expect_err(&format!("{field} = 0 must not parse"));
            assert!(
                err.to_string().contains(&format!("{field} must not be 0")),
                "{field}: {err}",
            );
        }
    }

    /// Zero stays legal where it is an enforceable cap rather than a
    /// wedge: a zero cap denies, it does not misconfigure.
    #[test]
    fn a_zero_deny_cap_stays_legal_and_resolves_to_zero() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[policy]
max_state_bytes = 0

[limits.http]
response_body_max_bytes = 0
"#,
        )
        .expect("zero deny caps parse");
        assert_eq!(cfg.policy.ceilings.max_state_bytes, 0);
        assert_eq!(cfg.limits.http.response_body_max_bytes, 0);
    }

    #[test]
    fn extensions_tables_parse_opaquely() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[extensions.example]
key = "value"
"#,
        )
        .expect("extensions table parses");
        let section = cfg.extensions.get("example").expect("example table");
        assert_eq!(section.get("key").and_then(|v| v.as_str()), Some("value"));
    }
}
