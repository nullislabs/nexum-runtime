//! Engine-side runtime configuration (`engine.toml`): chain RPC
//! endpoints, local-store location, and per-module resource limits.
//! Distinct from a module's `component.toml` manifest.
//!
//! Load order: `--engine-config` path, else `engine.toml` in the cwd,
//! else defaults (no chains, `state_dir = ./data`).

use std::collections::HashMap;
use std::net::IpAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_chains::Chain;
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

use crate::host_pattern::HostPattern;
use crate::runtime::dispatch_rate::{
    DEFAULT_DISPATCH_BURST, DEFAULT_DISPATCH_REFILL_PER_SEC, DispatchRatePolicy,
};
use crate::runtime::poison_policy::{POISON_MAX_FAILURES, POISON_WINDOW, PoisonPolicy};

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

/// Default per-caller submission budget within [`DEFAULT_QUOTA_WINDOW`].
pub const DEFAULT_QUOTA_MAX_CHARGES: u32 = 256;
/// Default sliding window the per-caller submission budget is counted over.
pub const DEFAULT_QUOTA_WINDOW: Duration = Duration::from_secs(60);
/// Default cap on receipts under status watch at once.
pub const DEFAULT_WATCH_MAX_ENTRIES: NonZeroUsize = nz_usize(1024);
/// Default base window a healthy provider refreshes within; the give-up
/// deadline is the derived `grace`, not this directly.
pub const DEFAULT_WATCH_EXPIRY: Duration = Duration::from_secs(86_400);
/// Derived grace defaults to this many `expiry` windows.
pub const WATCH_GRACE_MULTIPLIER: u64 = 2;
/// Ceiling on the derived grace window.
pub const WATCH_GRACE_MAX: Duration = Duration::from_secs(86_400);

/// Give-up window derived from `expiry`: `min(MULTIPLIER * expiry, MAX)`.
const fn derive_grace(expiry: Duration) -> Duration {
    let scaled = expiry.as_secs().saturating_mul(WATCH_GRACE_MULTIPLIER);
    let capped = if scaled < WATCH_GRACE_MAX.as_secs() {
        scaled
    } else {
        WATCH_GRACE_MAX.as_secs()
    };
    Duration::from_secs(capped)
}

/// Per-caller submission quota toward providers. A submission and a
/// charged decode failure each consume one unit; the window slides.
/// Resolved from `[limits.quota]`.
#[derive(Debug, Clone, Copy)]
pub struct SubmitQuota {
    /// Maximum charges a single caller may accrue within `window`.
    pub max_charges: u32,
    /// Sliding window the charges are counted across.
    pub window: Duration,
}

impl SubmitQuota {
    /// Budget paired with the window it is counted over.
    pub const fn new(max_charges: u32, window: Duration) -> Self {
        Self {
            max_charges,
            window,
        }
    }
}

impl Default for SubmitQuota {
    fn default() -> Self {
        Self::new(DEFAULT_QUOTA_MAX_CHARGES, DEFAULT_QUOTA_WINDOW)
    }
}

/// Bounds on a provider status-watch set: `max_entries` caps the
/// per-cadence poll fan-out, `grace` is the give-up deadline, `expiry`
/// the base window it derives from. Resolved from `[limits.watch]`.
#[derive(Debug, Clone, Copy)]
pub struct WatchLimit {
    /// Maximum receipts under status watch at once.
    pub max_entries: NonZeroUsize,
    /// Base window a healthy provider refreshes the deadline within.
    pub expiry: Duration,
    /// Give-up deadline: how long a watch survives an unreachable provider
    /// before unreported eviction. A reachable poll resets it; a resolve
    /// failure or errored poll rides out against it. Derived unless set.
    pub grace: Duration,
}

impl WatchLimit {
    /// Pair a cap with the base expiry; `grace` derives from `expiry`.
    pub const fn new(max_entries: NonZeroUsize, expiry: Duration) -> Self {
        Self::with_grace(max_entries, expiry, derive_grace(expiry))
    }

    /// As [`new`](Self::new) but with an explicit `grace`.
    pub const fn with_grace(max_entries: NonZeroUsize, expiry: Duration, grace: Duration) -> Self {
        Self {
            max_entries,
            expiry,
            grace,
        }
    }
}

impl Default for WatchLimit {
    fn default() -> Self {
        Self::new(DEFAULT_WATCH_MAX_ENTRIES, DEFAULT_WATCH_EXPIRY)
    }
}

/// Errors surfaced by [`load_or_default`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineConfigError {
    /// Failed to read the config file from disk.
    #[error("read engine config: {0}")]
    Io(#[from] std::io::Error),
    /// Config file was unparseable as TOML.
    #[error("parse engine config: {0}")]
    Toml(#[from] toml::de::Error),
    /// `${VAR}` env-var substitution failed (missing, malformed, or unclosed).
    #[error("engine config env-var substitution failed: {0}")]
    Substitute(#[from] EnvVarError),
    /// A `[chains.<key>]` key that is neither a numeric chain id nor a
    /// known chain name.
    #[error("engine config: [chains] key {key:?} is not a chain id or known chain name")]
    InvalidChainKey {
        /// The key as written.
        key: String,
    },
    /// A zero in a numeric field whose mechanism a zero would disable.
    #[error("engine config: {field} must not be 0")]
    ZeroField {
        /// TOML path of the refused field.
        field: String,
    },
}

/// Engine-side configuration loaded from `engine.toml`. Deserialization
/// goes through a raw shape whose `TryFrom` conversion validates the
/// `[chains]` keys, so this type never carries an unvalidated key.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "RawEngineConfig")]
pub struct EngineConfig {
    /// Process-wide settings: state directory, log level, metrics.
    pub engine: EngineSection,
    /// Per-module wasmtime resource limits, resolved once at load and
    /// applied uniformly.
    pub limits: ResolvedModuleLimits,
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
    /// Service components the supervisor boots alongside modules. Like a
    /// module, but the operator, not the author, scopes its transport here.
    pub services: Vec<ServiceEntry>,
    /// True when [`load_or_default`] found no engine.toml.
    pub defaulted: bool,
}

/// Raw deserialized engine config; the `[chains]` keys stay as written
/// until the `TryFrom` conversion into [`EngineConfig`] validates them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngineConfig {
    #[serde(default)]
    engine: EngineSection,
    #[serde(default)]
    limits: ModuleLimits,
    #[serde(default)]
    chains: HashMap<String, ChainConfig>,
    #[serde(default)]
    extensions: HashMap<String, toml::Value>,
    #[serde(default)]
    modules: Vec<ModuleEntry>,
    #[serde(default)]
    services: Vec<ServiceEntry>,
}

impl TryFrom<RawEngineConfig> for EngineConfig {
    type Error = EngineConfigError;

    /// The value checks serde defers: parse each raw `[chains]` key into a
    /// [`Chain`], refuse a zero timeout or `[limits]` knob, typed refusal
    /// instead of a serde string. The derived `Deserialize` runs this too,
    /// so the public `toml::from_str` path cannot yield an unvalidated
    /// config.
    fn try_from(raw: RawEngineConfig) -> Result<Self, EngineConfigError> {
        let mut chains = HashMap::with_capacity(raw.chains.len());
        for (key, cfg) in raw.chains {
            match key.parse::<Chain>() {
                Ok(chain) => {
                    // A zero timeout would leave every request unbounded.
                    if cfg.request_timeout_secs == 0 {
                        return Err(zero_field(&format!("chains.{key}.request_timeout_secs")));
                    }
                    chains.insert(chain, cfg);
                }
                Err(_) => return Err(EngineConfigError::InvalidChainKey { key }),
            }
        }
        Ok(Self {
            engine: raw.engine,
            limits: raw.limits.try_into()?,
            chains,
            extensions: raw.extensions,
            modules: raw.modules,
            services: raw.services,
            defaulted: false,
        })
    }
}

/// One `[[modules]]` table. `manifest` defaults to a sibling
/// `component.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleEntry {
    /// Path to the compiled `.wasm` component.
    pub path: std::path::PathBuf,
    /// Path to the module's `component.toml`. Defaults to `<path-parent>/component.toml`.
    #[serde(default)]
    pub manifest: Option<std::path::PathBuf>,
}

/// One `[[services]]` table. `path`/`manifest` mirror [`ModuleEntry`].
/// `http_allow` is the operator's transport grant: an empty list denies all
/// outbound HTTP.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceEntry {
    /// Path to the compiled `.wasm` service component.
    pub path: std::path::PathBuf,
    /// Path to the service's `component.toml`. Defaults to `<path-parent>/component.toml`.
    #[serde(default)]
    pub manifest: Option<std::path::PathBuf>,
    /// Outbound HTTP host allowlist: exact hostname or `*.suffix` wildcard,
    /// parsed to [`HostPattern`] as the config loads.
    #[serde(default)]
    pub http_allow: Vec<HostPattern>,
}

/// `[engine]`: settings that apply to the process, not to one module.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// One `[chains.<id>]` table: how the engine reaches a single chain.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    /// JSON-RPC endpoint. `ws(s)://` engages pubsub (needed for
    /// `eth_subscribe`); `http(s)://` is request/response only.
    pub rpc_url: String,
    /// Per-request timeout in seconds; HTTP bounds every call, WS only
    /// `chain::request`. Default 30, zero refused at load: it would leave
    /// every request unbounded.
    #[serde(default = "default_chain_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_chain_request_timeout_secs() -> u64 {
    30
}

/// Default fuel budget per `on_event` invocation (~1e9 WASM instructions).
const DEFAULT_FUEL_PER_EVENT: NonZeroU64 = nz_u64(1_000_000_000);

/// Default per-dispatch wall-clock deadline.
const DEFAULT_EVENT_DEADLINE: Duration = Duration::from_secs(120);

/// Default linear-memory cap per module store (64 MiB).
const DEFAULT_MEMORY_LIMIT: NonZeroUsize = nz_usize(64 * 1024 * 1024);

/// Default per-module local-store byte quota (50 MiB).
const DEFAULT_STATE_BYTES: u64 = 50 * 1024 * 1024;

/// Default ceiling on the guest-settable connect timeout.
const DEFAULT_HTTP_CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(10);

/// Default ceiling on the guest-settable first-byte timeout.
const DEFAULT_HTTP_FIRST_BYTE_TIMEOUT_MAX: Duration = Duration::from_secs(30);

/// Default ceiling on the guest-settable between-bytes timeout.
const DEFAULT_HTTP_BETWEEN_BYTES_TIMEOUT_MAX: Duration = Duration::from_secs(30);

/// Default total deadline on one outgoing exchange, connect through body
/// streaming.
const DEFAULT_HTTP_TOTAL_DEADLINE: Duration = Duration::from_secs(60);

/// Default cap on one incoming response body (16 MiB).
const DEFAULT_HTTP_RESPONSE_BODY_MAX: u64 = 16 * 1024 * 1024;

/// Default cap on one chain JSON-RPC response body (1 MiB).
const DEFAULT_CHAIN_RESPONSE_MAX_BYTES: NonZeroUsize = nz_usize(1024 * 1024);

/// Ceiling for the `[limits.http]` millisecond knobs (24 h).
const HTTP_LIMIT_MS_MAX: u64 = 86_400_000;

/// Default per-run log ring budget (256 KiB).
const DEFAULT_LOG_BYTES_PER_RUN: NonZeroUsize = nz_usize(256 * 1024);

/// Default past runs retained per module (16).
const DEFAULT_LOG_RUNS_RETAINED: NonZeroUsize = nz_usize(16);

/// Serde shape of `[limits]`. Every field is optional; conversion into
/// [`ResolvedModuleLimits`] fills the built-in defaults and refuses
/// zeroes. Sections are documented on their own types.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleLimits {
    /// Fuel budget granted per `on_event` invocation.
    pub fuel_per_event: Option<u64>,
    /// Wall-clock deadline (s) for a dispatch, covering host-call time fuel cannot meter.
    pub event_deadline_secs: Option<u64>,
    /// Linear-memory cap in bytes per module store.
    pub memory_bytes: Option<usize>,
    /// Local-store on-disk byte quota per module.
    pub state_bytes: Option<u64>,
    /// Outbound wasi:http limits.
    #[serde(default)]
    pub http: HttpLimitsSection,
    /// Chain JSON-RPC response size limits.
    #[serde(default)]
    pub chain: ChainLimitsSection,
    /// Per-run log retention limits.
    #[serde(default)]
    pub logs: LogLimitsSection,
    /// Poison-pill quarantine thresholds.
    #[serde(default)]
    pub poison: PoisonLimitsSection,
    /// Per-caller provider submission quota.
    #[serde(default)]
    pub quota: QuotaLimitsSection,
    /// Status-watch set bounds.
    #[serde(default)]
    pub watch: WatchLimitsSection,
    /// Per-module dispatch rate-limit thresholds.
    #[serde(default)]
    pub dispatch: DispatchLimitsSection,
}

/// `[limits]` resolved once at load: every optional knob replaced by its
/// override or built-in default. The [`TryFrom<ModuleLimits>`] conversion
/// refuses zeroes, so no consumer clamps on read.
#[derive(Debug, Clone)]
pub struct ResolvedModuleLimits {
    /// Fuel budget granted per `on_event` invocation.
    pub fuel_per_event: NonZeroU64,
    /// Wall-clock deadline for a dispatch, covering host-call time fuel
    /// cannot meter.
    pub event_deadline: Duration,
    /// Linear-memory cap in bytes per module store.
    pub memory_bytes: NonZeroUsize,
    /// Local-store on-disk byte quota per module; zero denies every write.
    pub state_bytes: u64,
    /// Cap on one chain JSON-RPC response body.
    pub chain_response_max_bytes: NonZeroUsize,
    /// Outbound wasi:http limits.
    pub http: OutboundHttpLimits,
    /// Addresses the operator permits despite falling in a refused range.
    /// Empty by default, so every refused range stays refused.
    pub http_permit_destinations: Vec<IpAddr>,
    /// Per-run log retention limits.
    pub logs: LogRetentionLimits,
    /// Poison-pill quarantine thresholds.
    pub poison: PoisonPolicy,
    /// Per-caller provider submission quota.
    pub quota: SubmitQuota,
    /// Status-watch set bounds.
    pub watch: WatchLimit,
    /// Per-module dispatch rate-limit policy.
    pub dispatch: DispatchRatePolicy,
}

impl Default for ResolvedModuleLimits {
    fn default() -> Self {
        match Self::try_from(ModuleLimits::default()) {
            Ok(resolved) => resolved,
            Err(_) => unreachable!("the built-in limit defaults are non-zero"),
        }
    }
}

/// A configured zero, named by its TOML path.
fn zero_field(field: &str) -> EngineConfigError {
    EngineConfigError::ZeroField {
        field: field.to_owned(),
    }
}

/// Override-or-default, proving the resolution in the type; a zero
/// override refuses, naming `field`.
fn nonzero_u64(
    field: &str,
    value: Option<u64>,
    default: NonZeroU64,
) -> Result<NonZeroU64, EngineConfigError> {
    match value {
        Some(v) => NonZeroU64::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// As [`nonzero_u64`], for `u32` knobs.
fn nonzero_u32(
    field: &str,
    value: Option<u32>,
    default: NonZeroU32,
) -> Result<NonZeroU32, EngineConfigError> {
    match value {
        Some(v) => NonZeroU32::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// As [`nonzero_u64`], for `usize` knobs.
fn nonzero_usize(
    field: &str,
    value: Option<usize>,
    default: NonZeroUsize,
) -> Result<NonZeroUsize, EngineConfigError> {
    match value {
        Some(v) => NonZeroUsize::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// Second-denominated knob resolved to a `Duration`, zero refused.
fn nonzero_secs(
    field: &str,
    value: Option<u64>,
    default: Duration,
) -> Result<Duration, EngineConfigError> {
    match value {
        Some(0) => Err(zero_field(field)),
        Some(secs) => Ok(Duration::from_secs(secs)),
        None => Ok(default),
    }
}

/// Millisecond knob resolved to a `Duration`: zero refused, a value above
/// [`HTTP_LIMIT_MS_MAX`] saturates down so timer arithmetic cannot
/// overflow at request time.
fn nonzero_ms_capped(
    field: &str,
    value: Option<u64>,
    default: Duration,
) -> Result<Duration, EngineConfigError> {
    match value {
        Some(0) => Err(zero_field(field)),
        Some(ms) => Ok(Duration::from_millis(ms.min(HTTP_LIMIT_MS_MAX))),
        None => Ok(default),
    }
}

impl TryFrom<ModuleLimits> for ResolvedModuleLimits {
    type Error = EngineConfigError;

    /// Refuses every zero that would disable the mechanism its field
    /// bounds; any other override resolves to exactly the value written.
    fn try_from(raw: ModuleLimits) -> Result<Self, EngineConfigError> {
        let http = OutboundHttpLimits {
            connect_timeout_max: nonzero_ms_capped(
                "limits.http.connect_timeout_max_ms",
                raw.http.connect_timeout_max_ms,
                DEFAULT_HTTP_CONNECT_TIMEOUT_MAX,
            )?,
            first_byte_timeout_max: nonzero_ms_capped(
                "limits.http.first_byte_timeout_max_ms",
                raw.http.first_byte_timeout_max_ms,
                DEFAULT_HTTP_FIRST_BYTE_TIMEOUT_MAX,
            )?,
            between_bytes_timeout_max: nonzero_ms_capped(
                "limits.http.between_bytes_timeout_max_ms",
                raw.http.between_bytes_timeout_max_ms,
                DEFAULT_HTTP_BETWEEN_BYTES_TIMEOUT_MAX,
            )?,
            total_deadline: nonzero_ms_capped(
                "limits.http.total_deadline_ms",
                raw.http.total_deadline_ms,
                DEFAULT_HTTP_TOTAL_DEADLINE,
            )?,
            // Zero stays legal: a zero cap refuses every response body,
            // which is an enforceable operator choice, not a wedge.
            response_body_max_bytes: raw
                .http
                .response_body_max_bytes
                .unwrap_or(DEFAULT_HTTP_RESPONSE_BODY_MAX),
        };
        let logs = LogRetentionLimits {
            bytes_per_run: nonzero_usize(
                "limits.logs.bytes_per_run",
                raw.logs.bytes_per_run,
                DEFAULT_LOG_BYTES_PER_RUN,
            )?,
            runs_retained: nonzero_usize(
                "limits.logs.runs_retained",
                raw.logs.runs_retained,
                DEFAULT_LOG_RUNS_RETAINED,
            )?,
        };
        let poison = PoisonPolicy::new(
            nonzero_u32(
                "limits.poison.max_failures",
                raw.poison.max_failures,
                POISON_MAX_FAILURES,
            )?,
            nonzero_secs(
                "limits.poison.window_secs",
                raw.poison.window_secs,
                POISON_WINDOW,
            )?,
        );
        let quota = SubmitQuota::new(
            // Zero stays legal: a zero budget denies every submission,
            // which is an enforceable operator choice, not a wedge.
            raw.quota.max_charges.unwrap_or(DEFAULT_QUOTA_MAX_CHARGES),
            nonzero_secs(
                "limits.quota.window_secs",
                raw.quota.window_secs,
                DEFAULT_QUOTA_WINDOW,
            )?,
        );
        let max_entries = nonzero_usize(
            "limits.watch.max_entries",
            raw.watch.max_entries,
            DEFAULT_WATCH_MAX_ENTRIES,
        )?;
        let expiry = nonzero_secs(
            "limits.watch.expiry_secs",
            raw.watch.expiry_secs,
            DEFAULT_WATCH_EXPIRY,
        )?;
        // An explicit grace overrides the give-up deadline, else it
        // derives from `expiry` via [`WatchLimit::new`].
        let watch = match raw.watch.grace_secs {
            Some(0) => return Err(zero_field("limits.watch.grace_secs")),
            Some(secs) => WatchLimit::with_grace(max_entries, expiry, Duration::from_secs(secs)),
            None => WatchLimit::new(max_entries, expiry),
        };
        let dispatch = DispatchRatePolicy::new(
            nonzero_u32(
                "limits.dispatch.burst",
                raw.dispatch.burst,
                DEFAULT_DISPATCH_BURST,
            )?,
            nonzero_u32(
                "limits.dispatch.refill_per_sec",
                raw.dispatch.refill_per_sec,
                DEFAULT_DISPATCH_REFILL_PER_SEC,
            )?,
        );
        Ok(Self {
            fuel_per_event: nonzero_u64(
                "limits.fuel_per_event",
                raw.fuel_per_event,
                DEFAULT_FUEL_PER_EVENT,
            )?,
            event_deadline: nonzero_secs(
                "limits.event_deadline_secs",
                raw.event_deadline_secs,
                DEFAULT_EVENT_DEADLINE,
            )?,
            memory_bytes: nonzero_usize(
                "limits.memory_bytes",
                raw.memory_bytes,
                DEFAULT_MEMORY_LIMIT,
            )?,
            // Zero stays legal: a zero quota denies every local-store
            // write, which is an enforceable operator choice.
            state_bytes: raw.state_bytes.unwrap_or(DEFAULT_STATE_BYTES),
            chain_response_max_bytes: nonzero_usize(
                "limits.chain.response_body_max_bytes",
                raw.chain.response_body_max_bytes.map(|b| b as usize),
                DEFAULT_CHAIN_RESPONSE_MAX_BYTES,
            )?,
            http,
            http_permit_destinations: raw.http.permit_destinations,
            logs,
            poison,
            quota,
            watch,
            dispatch,
        })
    }
}

/// `[limits.http]` outbound limits. All optional; a zero millisecond value
/// refuses at load, one above 24 h saturates down. The `*_timeout_max_ms`
/// fields are ceilings on the matching guest-settable `request-options`
/// timeouts: a higher guest value is clamped down, an unset one inherits
/// the ceiling.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpLimitsSection {
    /// Ceiling on the guest-settable connect timeout, in milliseconds.
    pub connect_timeout_max_ms: Option<u64>,
    /// Ceiling on the guest-settable first-byte timeout, in milliseconds.
    pub first_byte_timeout_max_ms: Option<u64>,
    /// Ceiling on the guest-settable between-bytes timeout, in milliseconds.
    pub between_bytes_timeout_max_ms: Option<u64>,
    /// Total deadline on one outgoing exchange (connect through body
    /// streaming), in milliseconds.
    pub total_deadline_ms: Option<u64>,
    /// Cap on one incoming response body, in bytes.
    pub response_body_max_bytes: Option<u64>,
    /// Addresses in otherwise-refused ranges that this deployment permits.
    /// The operator writes these; a module manifest cannot add to them.
    #[serde(default)]
    pub permit_destinations: Vec<IpAddr>,
}

/// `[limits.chain]` chain JSON-RPC response size limit. Optional; defaults
/// to 1 MiB.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainLimitsSection {
    /// Cap on one chain JSON-RPC response body, in bytes.
    pub response_body_max_bytes: Option<u64>,
}

/// Resolved outbound HTTP limits the wasi:http gate enforces per request.
#[derive(Debug, Clone, Copy)]
pub struct OutboundHttpLimits {
    /// Ceiling on the guest-settable connect timeout.
    pub connect_timeout_max: Duration,
    /// Ceiling on the guest-settable first-byte timeout.
    pub first_byte_timeout_max: Duration,
    /// Ceiling on the guest-settable between-bytes timeout.
    pub between_bytes_timeout_max: Duration,
    /// Total deadline on one exchange, connect through body streaming.
    pub total_deadline: Duration,
    /// Cap on one incoming response body.
    pub response_body_max_bytes: u64,
}

/// `[limits.logs]` per-run retention knobs. Both optional; a zero refuses
/// at load.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogLimitsSection {
    /// Byte budget for one run's in-memory ring.
    pub bytes_per_run: Option<usize>,
    /// Number of past runs retained per module.
    pub runs_retained: Option<usize>,
}

/// `[limits.poison]` quarantine thresholds. Both optional; a zero refuses
/// at load. A module reaching `max_failures` traps within
/// a sliding `window_secs` is quarantined and no longer dispatched until
/// an operator-driven engine restart.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoisonLimitsSection {
    /// Maximum traps within the window before a module is poisoned.
    pub max_failures: Option<u32>,
    /// Sliding window the traps are counted across, in seconds.
    pub window_secs: Option<u64>,
}

/// `[limits.quota]` per-caller submission budget. Both optional. A caller
/// (keyed by its namespace) may accrue at most `max_charges` within a
/// sliding `window_secs`; a charged decode failure counts the same.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimitsSection {
    /// Maximum submissions (plus charged decode failures) per caller in the
    /// window.
    pub max_charges: Option<u32>,
    /// Sliding window the charges are counted across, in seconds.
    pub window_secs: Option<u64>,
}

/// `[limits.watch]` status-watch set bounds. All optional; a zero refuses
/// at load. The cap bounds the per-cadence poll fan-out; at the cap a new
/// watch is refused and logged, live watches are never dropped.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchLimitsSection {
    /// Maximum receipts under status watch at once.
    pub max_entries: Option<usize>,
    /// Base window seconds a healthy venue refreshes the deadline within.
    pub expiry_secs: Option<u64>,
    /// Give-up deadline seconds: how long a watch rides out an unreachable
    /// venue before eviction. Omitted, it derives from `expiry_secs`.
    pub grace_secs: Option<u64>,
}

/// `[limits.dispatch]` per-module dispatch rate-limit knobs. Both
/// optional; omitted values resolve to the production defaults, and a
/// zero refuses at load.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchLimitsSection {
    /// Burst allowance: the token-bucket capacity.
    pub burst: Option<u32>,
    /// Sustained dispatch ceiling: tokens replenished per second.
    pub refill_per_sec: Option<u32>,
}

/// Resolved log retention limits the in-memory store enforces. Non-zero
/// by type: a zero budget would retain nothing.
#[derive(Debug, Clone, Copy)]
pub struct LogRetentionLimits {
    /// Byte budget for one run's ring; the newest record is never evicted
    /// to nothing.
    pub bytes_per_run: NonZeroUsize,
    /// Runs retained per module; the oldest run evicts first.
    pub runs_retained: NonZeroUsize,
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_log_level() -> String {
    "info".to_owned()
}

/// Read an engine config from disk, returning defaults if the file is
/// missing. Parse errors propagate via [`EngineConfigError`].
pub fn load_or_default(path: Option<&Path>) -> Result<EngineConfig, EngineConfigError> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("engine.toml"),
    };

    if !path.exists() {
        warn!(
            path = %path.display(),
            "engine.toml not found - running with defaults (no chain RPC endpoints; \
             chain-backed host calls will return Unsupported)"
        );
        return Ok(EngineConfig {
            defaulted: true,
            ..EngineConfig::default()
        });
    }

    let raw = std::fs::read_to_string(&path)?;
    // Operators reference RPC URLs (which carry API keys) via
    // `${VAR_NAME}` placeholders so the committed `engine.toml` /
    // `engine.docker.toml` stays secret-free. The substitution runs
    // before TOML parse so a missing var fails fast with the exact
    // variable name, not a downstream "invalid URI" several layers
    // deep.
    let substituted = substitute_env_vars(&raw)?;
    // Parse the raw shape, then convert, so a bad `[chains]` key surfaces
    // as the typed `InvalidChainKey` rather than erased into a serde
    // string by the derived `Deserialize`.
    let cfg = EngineConfig::try_from(toml::from_str::<RawEngineConfig>(&substituted)?)?;
    info!(
        path = %path.display(),
        chains = cfg.chains.len(),
        state_dir = %cfg.engine.state_dir.display(),
        "engine config loaded",
    );
    Ok(cfg)
}

/// Replace every `${VAR_NAME}` token in `raw` with its environment value,
/// erroring on any missing variable. Recognised names match
/// `[A-Z_][A-Z0-9_]*`; anything else inside `${...}` is rejected.
fn substitute_env_vars(raw: &str) -> Result<String, EnvVarError> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find the closing `}`.
            let start = i + 2;
            let Some(end_offset) = raw[start..].find('}') else {
                return Err(EnvVarError::Unclosed { offset: i });
            };
            let end = start + end_offset;
            let name = &raw[start..end];
            if !is_valid_env_name(name) {
                return Err(EnvVarError::InvalidName {
                    name: name.to_owned(),
                });
            }
            match std::env::var(name) {
                Ok(val) => out.push_str(&val),
                Err(_) => {
                    return Err(EnvVarError::Missing {
                        name: name.to_owned(),
                    });
                }
            }
            i = end + 1;
        } else {
            // Push one UTF-8 char (find the next char boundary).
            #[expect(
                clippy::expect_used,
                reason = "i only ever advances by ch.len_utf8() or past an ASCII '}', so raw[i..] starts on a char boundary and is non-empty inside the loop"
            )]
            let ch = raw[i..]
                .chars()
                .next()
                .expect("byte index is on char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

fn is_valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_uppercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Errors from `${VAR}` substitution in `engine.toml`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvVarError {
    /// A referenced variable is absent from the process environment.
    /// Substitution refuses rather than expanding to empty, because an
    /// empty RPC URL fails later and further from the cause.
    #[error(
        "environment variable `{name}` referenced via ${{{name}}} in engine.toml but not set. \
         Export it before launching the engine (e.g. via a `.env` file consumed by `docker compose`)."
    )]
    Missing {
        /// The variable as referenced.
        name: String,
    },
    /// The name inside `${...}` is not a shell-style variable name. The
    /// message guesses an upper-case spelling, which is the usual slip.
    #[error(
        "invalid env var name `{name}` inside ${{...}} in engine.toml - names must match \
         [A-Z_][A-Z0-9_]*. Typo, or did you mean `${{{name_upper}}}`?",
        name_upper = name.to_uppercase()
    )]
    InvalidName {
        /// The rejected name, as written.
        name: String,
    },
    /// A `${` with no closing brace before the end of the file.
    #[error(
        "unclosed `${{` at byte offset {offset} in engine.toml - every `${{` needs a matching `}}`."
    )]
    Unclosed {
        /// Byte offset of the opening `${` that never closed.
        offset: usize,
    },
}

/// Blank the credential-bearing parts of a URL (userinfo, query, fragment,
/// long API-key path segments) so it is safe to log. Unparseable input
/// yields a placeholder.
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return "<unparseable-url>".to_owned();
    };
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("REDACTED");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(Some("REDACTED"));
    }
    // Key-in-path shape (Alchemy/Infura): a >20-char segment with no '.'/':' is
    // an API key. Collect owned first - can't hold the read + write borrows.
    let redacted: Option<Vec<String>> = parsed.path_segments().map(|segs| {
        segs.map(|seg| {
            if seg.len() > 20 && !seg.contains('.') && !seg.contains(':') {
                "KEY".to_owned()
            } else {
                seg.to_owned()
            }
        })
        .collect()
    });
    if let Some(segments) = redacted
        && let Ok(mut pm) = parsed.path_segments_mut()
    {
        pm.clear();
        for seg in &segments {
            pm.push(seg);
        }
    }
    if parsed.query().is_some() {
        parsed.set_query(Some("REDACTED"));
    }
    if parsed.fragment().is_some() {
        parsed.set_fragment(Some("REDACTED"));
    }
    parsed.to_string()
}

/// For text an untrusted reader sees. [`redact_url`] is not enough: it keeps
/// the authority and short path segments, either of which can hold the key.
pub fn redact_urls_in_text(text: &str) -> String {
    const PLACEHOLDER: &str = "<redacted-url>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        // Cut past the whole char: the text is server-controlled, and
        // `panic = "abort"` makes a mid-codepoint slice fatal to the host.
        let scheme_start = rest[..sep]
            .char_indices()
            .rev()
            .find(|&(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let after = &rest[sep + 3..];
        let token_end = sep + 3 + after.find(char::is_whitespace).unwrap_or(after.len());
        let token = &rest[scheme_start..token_end];
        let url = token.trim_end_matches([')', ']', '}', '.', ',', ';', '\'', '"', '>']);
        out.push_str(&rest[..scheme_start]);
        out.push_str(PLACEHOLDER);
        out.push_str(&token[url.len()..]);
        rest = &rest[token_end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            cfg.chains
                .get(&Chain::sepolia())
                .expect("sepolia entry")
                .rpc_url,
            "wss://example.test/sepolia",
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
    fn load_or_default_marks_a_missing_file_as_defaulted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("engine.toml");
        let cfg = load_or_default(Some(&missing)).expect("a missing file falls back to defaults");
        assert!(
            cfg.defaulted,
            "the missing-file fallback carries provenance"
        );
        assert!(cfg.chains.is_empty());
    }

    #[test]
    fn load_or_default_marks_a_loaded_file_as_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.toml");
        std::fs::write(&path, "[chains.1]\nrpc_url = \"http://localhost:8545\"\n")
            .expect("write engine.toml");
        let cfg = load_or_default(Some(&path)).expect("the file parses");
        assert!(!cfg.defaulted, "a loaded engine.toml is not defaulted");
        assert_eq!(cfg.chains.len(), 1);
    }

    #[test]
    fn require_component_digest_defaults_false_and_parses() {
        assert!(!EngineConfig::default().engine.require_component_digest);
        let cfg: EngineConfig = toml::from_str("[engine]\nrequire_component_digest = true\n")
            .expect("the [engine] flag parses");
        assert!(cfg.engine.require_component_digest);
    }

    #[test]
    fn http_limits_default_when_absent() {
        let http = ResolvedModuleLimits::default().http;
        assert_eq!(http.connect_timeout_max, Duration::from_secs(10));
        assert_eq!(http.first_byte_timeout_max, Duration::from_secs(30));
        assert_eq!(http.between_bytes_timeout_max, Duration::from_secs(30));
        assert_eq!(http.total_deadline, Duration::from_secs(60));
        assert_eq!(http.response_body_max_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn http_limits_parse_with_partial_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits]
fuel_per_event = 7

[limits.http]
connect_timeout_max_ms  = 5_000
total_deadline_ms       = 90_000
response_body_max_bytes = 1_024
"#,
        )
        .expect("limits.http parses");
        assert_eq!(cfg.limits.fuel_per_event.get(), 7);
        let http = cfg.limits.http;
        assert_eq!(http.connect_timeout_max, Duration::from_millis(5_000));
        assert_eq!(http.total_deadline, Duration::from_millis(90_000));
        assert_eq!(http.response_body_max_bytes, 1_024);
        // Unset fields keep the built-in defaults.
        assert_eq!(http.first_byte_timeout_max, Duration::from_secs(30));
        assert_eq!(http.between_bytes_timeout_max, Duration::from_secs(30));
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
                "[[modules]]\npath = \"m.wasm\"\nmanifets = \"c.toml\"\n",
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

[limits]
fuel_per_event = 7

[limits.http]
total_deadline_ms = 1000

[chains.1]
rpc_url = "https://example.test"

[extensions.acme]
anything = "goes here, the engine never reads it"

[[modules]]
path = "m.wasm"

[[services]]
path = "s.wasm"
http_allow = ["api.acme.example"]
"#,
        )
        .expect("every documented section parses under the guard");
        assert_eq!(cfg.limits.fuel_per_event.get(), 7);
        assert_eq!(cfg.modules.len(), 1);
        assert_eq!(cfg.services.len(), 1);
        assert!(
            cfg.extensions.contains_key("acme"),
            "an extension table stays opaque and unguarded",
        );
    }

    #[test]
    fn permit_destinations_defaults_to_empty_and_parses_both_families() {
        let bare: EngineConfig = toml::from_str("[limits]\n").expect("bare limits parse");
        assert!(
            bare.limits.http_permit_destinations.is_empty(),
            "an absent list permits nothing"
        );

        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.http]
permit_destinations = ["10.0.5.7", "::1"]
"#,
        )
        .expect("permit_destinations parses");
        assert_eq!(
            cfg.limits.http_permit_destinations,
            vec![
                IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 5, 7)),
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ]
        );
    }

    #[test]
    fn permit_destinations_refuses_a_value_that_is_not_an_address() {
        let err = toml::from_str::<EngineConfig>(
            r#"
[limits.http]
permit_destinations = ["10.0.5.0/24"]
"#,
        )
        .expect_err("a CIDR range is not an address");
        // Serde's own type mismatch, not a message we threaded through it.
        assert!(err.to_string().contains("permit_destinations"), "{err}");
    }

    #[test]
    fn chain_limits_default_when_absent() {
        assert_eq!(
            ResolvedModuleLimits::default()
                .chain_response_max_bytes
                .get(),
            1024 * 1024,
        );
    }

    #[test]
    fn chain_limits_parse_with_override() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.chain]
response_body_max_bytes = 2_048
"#,
        )
        .expect("limits.chain parses");
        assert_eq!(cfg.limits.chain_response_max_bytes.get(), 2_048);
    }

    /// Pins the built-in numbers, not the constants, so a resolution that
    /// pairs a field with the wrong default constant fails here.
    #[test]
    fn core_limits_default_when_absent() {
        let limits = ResolvedModuleLimits::default();
        assert_eq!(limits.fuel_per_event.get(), 1_000_000_000);
        assert_eq!(limits.event_deadline, Duration::from_secs(120));
        assert_eq!(limits.memory_bytes.get(), 64 * 1024 * 1024);
        assert_eq!(limits.state_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn core_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits]
fuel_per_event      = 7
event_deadline_secs = 30
memory_bytes        = 1_048_576
state_bytes         = 2_048
"#,
        )
        .expect("top-level limits parse");
        assert_eq!(cfg.limits.fuel_per_event.get(), 7);
        assert_eq!(cfg.limits.event_deadline, Duration::from_secs(30));
        assert_eq!(cfg.limits.memory_bytes.get(), 1_048_576);
        assert_eq!(cfg.limits.state_bytes, 2_048);
    }

    #[test]
    fn quota_limits_default_when_absent() {
        let quota = ResolvedModuleLimits::default().quota;
        assert_eq!(quota.max_charges, 256);
        assert_eq!(quota.window, Duration::from_secs(60));
    }

    #[test]
    fn quota_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.quota]
max_charges = 9
window_secs = 30
"#,
        )
        .expect("limits.quota parses");
        assert_eq!(cfg.limits.quota.max_charges, 9);
        assert_eq!(cfg.limits.quota.window, Duration::from_secs(30));
    }

    /// Every `[limits]` zero that used to saturate silently now refuses at
    /// load, through the typed conversion and the public parse alike.
    #[test]
    fn a_zero_limit_refuses_at_load_naming_the_field() {
        for (toml, field) in [
            ("[limits]\nfuel_per_event = 0\n", "limits.fuel_per_event"),
            (
                "[limits]\nevent_deadline_secs = 0\n",
                "limits.event_deadline_secs",
            ),
            ("[limits]\nmemory_bytes = 0\n", "limits.memory_bytes"),
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
            (
                "[limits.quota]\nwindow_secs = 0\n",
                "limits.quota.window_secs",
            ),
            (
                "[limits.watch]\nmax_entries = 0\n",
                "limits.watch.max_entries",
            ),
            (
                "[limits.watch]\nexpiry_secs = 0\n",
                "limits.watch.expiry_secs",
            ),
            (
                "[limits.watch]\ngrace_secs = 0\n",
                "limits.watch.grace_secs",
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
    /// wedge: a zero quota denies, it does not misconfigure.
    #[test]
    fn a_zero_deny_cap_stays_legal_and_resolves_to_zero() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits]
state_bytes = 0

[limits.http]
response_body_max_bytes = 0

[limits.quota]
max_charges = 0
"#,
        )
        .expect("zero deny caps parse");
        assert_eq!(cfg.limits.state_bytes, 0);
        assert_eq!(cfg.limits.http.response_body_max_bytes, 0);
        assert_eq!(cfg.limits.quota.max_charges, 0);
    }

    #[test]
    fn http_limits_saturate_a_millisecond_value_above_the_ceiling() {
        // u64::MAX would overflow timer arithmetic at request time, so it
        // saturates down to the 24 h ceiling at load.
        let limits = ModuleLimits {
            http: HttpLimitsSection {
                total_deadline_ms: Some(u64::MAX),
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = ResolvedModuleLimits::try_from(limits).expect("saturating value resolves");
        assert_eq!(
            resolved.http.total_deadline,
            Duration::from_millis(86_400_000)
        );
    }

    #[test]
    fn log_limits_default_when_absent() {
        let logs = ResolvedModuleLimits::default().logs;
        assert_eq!(logs.bytes_per_run.get(), 256 * 1024);
        assert_eq!(logs.runs_retained.get(), 16);
    }

    #[test]
    fn log_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.logs]
bytes_per_run = 4_096
runs_retained = 3
"#,
        )
        .expect("limits.logs parses");
        let logs = cfg.limits.logs;
        assert_eq!(logs.bytes_per_run.get(), 4_096);
        assert_eq!(logs.runs_retained.get(), 3);
    }

    #[test]
    fn poison_limits_default_when_absent() {
        let poison = ResolvedModuleLimits::default().poison;
        assert_eq!(poison.max_failures, POISON_MAX_FAILURES);
        assert_eq!(poison.window, POISON_WINDOW);
    }

    #[test]
    fn poison_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.poison]
max_failures = 3
window_secs  = 60
"#,
        )
        .expect("limits.poison parses");
        let poison = cfg.limits.poison;
        assert_eq!(poison.max_failures.get(), 3);
        assert_eq!(poison.window, Duration::from_secs(60));
    }

    #[test]
    fn adapters_parse_with_scoped_transport_grants() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[[services]]
path = "providers/acme/acme_provider.wasm"
http_allow = ["api.acme.example", "*.acme.example"]

[[services]]
path = "services/bare/bare.wasm"
manifest = "services/bare/component.toml"
"#,
        )
        .expect("services parse");
        assert_eq!(cfg.services.len(), 2);
        let first = &cfg.services[0];
        assert_eq!(
            first.path,
            PathBuf::from("providers/acme/acme_provider.wasm")
        );
        assert!(first.manifest.is_none(), "manifest defaults to sibling");
        assert_eq!(
            first.http_allow,
            vec![
                HostPattern::from("api.acme.example"),
                HostPattern::from("*.acme.example"),
            ]
        );
        let second = &cfg.services[1];
        assert_eq!(
            second.manifest.as_deref(),
            Some(Path::new("services/bare/component.toml"))
        );
        assert!(
            second.http_allow.is_empty(),
            "unset scope grants default empty",
        );
    }

    #[test]
    fn adapters_default_empty_when_absent() {
        let cfg = EngineConfig::default();
        assert!(cfg.services.is_empty());
    }

    #[test]
    fn dispatch_rate_default_when_absent() {
        let policy = ResolvedModuleLimits::default().dispatch;
        assert_eq!(policy.capacity, DEFAULT_DISPATCH_BURST);
        assert_eq!(policy.refill_per_sec, DEFAULT_DISPATCH_REFILL_PER_SEC);
    }

    #[test]
    fn dispatch_rate_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.dispatch]
burst          = 8
refill_per_sec = 4
"#,
        )
        .expect("limits.dispatch parses");
        let policy = cfg.limits.dispatch;
        assert_eq!(policy.capacity.get(), 8);
        assert_eq!(policy.refill_per_sec.get(), 4);
    }

    #[test]
    fn watch_limits_default_when_absent() {
        let watch = ResolvedModuleLimits::default().watch;
        assert_eq!(watch.max_entries, DEFAULT_WATCH_MAX_ENTRIES);
        assert_eq!(watch.expiry, DEFAULT_WATCH_EXPIRY);
    }

    #[test]
    fn watch_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.watch]
max_entries = 32
expiry_secs = 900
"#,
        )
        .expect("limits.watch parses");
        let watch = cfg.limits.watch;
        assert_eq!(watch.max_entries.get(), 32);
        assert_eq!(watch.expiry, Duration::from_secs(900));
        // Omitted grace_secs derives from expiry (min(2 * expiry, 24h)).
        assert_eq!(watch.grace, Duration::from_secs(1800));

        // An explicit grace_secs overrides the derivation.
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.watch]
expiry_secs = 900
grace_secs = 120
"#,
        )
        .expect("limits.watch parses");
        assert_eq!(cfg.limits.watch.grace, Duration::from_secs(120));
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

    #[test]
    fn redact_replaces_long_path_segments() {
        let redacted =
            redact_url("https://lb.drpc.live/sepolia/AnOfyGnZ_0nWpS-OOwQzqAnFj_Naa0sR8ZxkVjewFaCJ");
        assert!(
            redacted.contains("KEY"),
            "long segment redacted: {redacted}"
        );
        assert!(
            !redacted.contains("AnOfyGnZ"),
            "the key must be gone: {redacted}",
        );
    }

    #[test]
    fn redact_keeps_short_segments_intact() {
        // Hostnames + "v2" path bits must not be redacted.
        let redacted = redact_url("https://eth-mainnet.g.alchemy.com/v2/abc");
        assert!(redacted.contains("eth-mainnet.g.alchemy.com"));
        assert!(redacted.contains("v2"));
    }

    #[test]
    fn redact_strips_userinfo_credentials() {
        // url renders userinfo as REDACTED:REDACTED@ when both parts are
        // present; assert the secret is gone rather than an exact string.
        let redacted = redact_url("https://user:pass@rpc.example.com/path");
        assert!(!redacted.contains("user:pass"), "userinfo gone: {redacted}");
        assert!(!redacted.contains("pass"), "password gone: {redacted}");
        assert!(
            redacted.contains("rpc.example.com"),
            "host kept: {redacted}"
        );
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn redact_strips_query_param_values() {
        let redacted = redact_url("https://rpc.example.com/v1?key=supersecret");
        assert!(
            !redacted.contains("supersecret"),
            "query secret gone: {redacted}"
        );
        assert!(redacted.contains("rpc.example.com"));
    }

    #[test]
    fn redact_strips_bare_query_flag() {
        // A bare `?token` flag (no `=`) is the whole query string; blanking
        // the query removes it. This is the gap string heuristics missed.
        let redacted = redact_url("https://rpc.example.com/v1?myapitoken");
        assert!(
            !redacted.contains("myapitoken"),
            "bare flag gone: {redacted}"
        );
        assert!(redacted.contains("rpc.example.com"));
    }

    #[test]
    fn redact_strips_fragment() {
        // OAuth-style bearer tokens can ride in the fragment.
        let redacted = redact_url("https://rpc.example.com/v1#bearertoken");
        assert!(
            !redacted.contains("bearertoken"),
            "fragment gone: {redacted}"
        );
        assert!(redacted.contains("rpc.example.com"));
    }

    #[test]
    fn redact_at_in_path_is_not_treated_as_userinfo() {
        // An `@` inside a path segment must not be parsed as userinfo; the
        // host stays intact.
        let redacted = redact_url("https://rpc.example.com/foo@bar/baz");
        assert!(
            redacted.contains("rpc.example.com"),
            "host kept: {redacted}"
        );
    }

    #[test]
    fn redact_leaves_clean_wss_url_intact() {
        // A url with no secret survives materially unchanged.
        let redacted = redact_url("wss://rpc.example.com/v1");
        assert!(redacted.contains("rpc.example.com"));
        assert!(redacted.contains("v1"));
        assert!(!redacted.contains("REDACTED"));
        assert!(!redacted.contains("KEY"));
    }

    #[test]
    fn redact_returns_placeholder_for_unparseable_url() {
        assert_eq!(redact_url("not a url"), "<unparseable-url>");
    }

    #[test]
    fn redact_text_handles_reqwest_parenthesized_form() {
        let text = "error sending request for url \
                    (https://lb.example.com/v2/AnOfyGnZ0nWpSOOwQzqAnFjNaa0s?apikey=qsecret)";
        let redacted = redact_urls_in_text(text);
        assert_eq!(redacted, "error sending request for url (<redacted-url>)");
    }

    #[test]
    fn redact_text_without_url_is_unchanged() {
        let text = "backend connection task has stopped";
        assert_eq!(redact_urls_in_text(text), text);
    }

    #[test]
    fn redact_text_replaces_unparseable_url_wholesale() {
        let redacted = redact_urls_in_text("connect to http://[secret failed");
        assert_eq!(redacted, "connect to <redacted-url> failed");
    }

    #[test]
    fn redact_text_handles_every_url_occurrence() {
        let text = "tried https://a.example/?k=one then wss://user:two@b.example/ws";
        let redacted = redact_urls_in_text(text);
        assert_eq!(redacted, "tried <redacted-url> then <redacted-url>");
    }

    #[test]
    fn redact_text_drops_host_borne_and_short_path_keys() {
        for text in [
            "error sending request for url (https://k7fQz2m9Xd.eth.rpc.example.com/)",
            "error sending request for url (https://rpc.example.com/k7fQz2m9Xd)",
        ] {
            let redacted = redact_urls_in_text(text);
            assert!(!redacted.contains("k7fQz2m9Xd"), "key gone: {redacted}");
            assert!(
                !redacted.contains("rpc.example.com"),
                "host gone: {redacted}"
            );
            assert_eq!(redacted, "error sending request for url (<redacted-url>)");
        }
    }

    #[test]
    fn redact_text_survives_a_multibyte_char_abutting_the_url() {
        for text in [
            "upstream \u{201c}https://rpc.example/v2/KEY\u{201d} is down",
            "upstream\u{a0}https://rpc.example/v2/KEY unavailable",
            "\u{9519}\u{8bef}\u{ff1a}https://rpc.example/v2/KEY",
            "caf\u{e9}://rpc.example/v2/KEY",
        ] {
            let redacted = redact_urls_in_text(text);
            assert!(!redacted.contains("rpc.example"), "url gone: {redacted}");
        }
    }

    //
    // These tests stash + restore process env vars under unique names
    // so parallel `cargo test` runs don't trip on each other.

    fn with_env<F: FnOnce()>(name: &str, value: &str, body: F) {
        let prev = std::env::var(name).ok();
        // SAFETY: tests are single-threaded within one test fn; setting
        // an env var here is fine since the unique-name convention
        // avoids cross-test races.
        unsafe { std::env::set_var(name, value) };
        body();
        match prev {
            Some(v) => unsafe { std::env::set_var(name, v) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    #[test]
    fn substitute_replaces_known_variable() {
        with_env("NEXUM_TEST_RPC", "wss://example.test/abc", || {
            let raw = r#"rpc_url = "${NEXUM_TEST_RPC}""#;
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, r#"rpc_url = "wss://example.test/abc""#);
        });
    }

    #[test]
    fn substitute_errors_on_missing_variable() {
        // Variable name must not collide with anything in the operator
        // environment. Use a guaranteed-unique prefix.
        let err =
            substitute_env_vars(r#"x = "${NEXUM_TEST_DEFINITELY_UNSET_VAR_XYZ}""#).unwrap_err();
        assert!(
            matches!(&err, EnvVarError::Missing { name }
                if name == "NEXUM_TEST_DEFINITELY_UNSET_VAR_XYZ"),
            "{err}"
        );
    }

    #[test]
    fn substitute_errors_on_invalid_name() {
        let err = substitute_env_vars(r#"x = "${lowercase_name}""#).unwrap_err();
        assert!(matches!(err, EnvVarError::InvalidName { .. }));
    }

    #[test]
    fn substitute_errors_on_unclosed_brace() {
        let err = substitute_env_vars(r#"x = "${UNCLOSED"#).unwrap_err();
        assert!(matches!(err, EnvVarError::Unclosed { .. }));
    }

    #[test]
    fn substitute_passes_text_with_no_placeholders_through() {
        let raw = "no placeholders here\nrpc_url = \"wss://x\"";
        assert_eq!(substitute_env_vars(raw).unwrap(), raw);
    }

    #[test]
    fn substitute_handles_multiple_placeholders_in_one_line() {
        with_env("NEXUM_TEST_A", "alpha", || {
            with_env("NEXUM_TEST_B", "beta", || {
                let raw = "k = \"${NEXUM_TEST_A}-${NEXUM_TEST_B}\"";
                let out = substitute_env_vars(raw).unwrap();
                assert_eq!(out, "k = \"alpha-beta\"");
            });
        });
    }

    #[test]
    fn substitute_preserves_utf8_around_placeholder() {
        // The hand-rolled byte loop must respect multi-byte UTF-8.
        with_env("NEXUM_TEST_U", "X", || {
            let raw = "# 河 ${NEXUM_TEST_U} ⚙️\n";
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, "# 河 X ⚙️\n");
        });
    }
}
