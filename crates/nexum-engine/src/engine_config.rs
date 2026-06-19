//! Engine-side runtime configuration.
//!
//! Distinct from `module.toml` (module manifest): this file describes
//! the *engine*'s I/O wiring - chain RPC endpoints and the on-disk
//! location of the `local-store` database. Both are required for the
//! 0.2 reference engine to do anything other than print stubs.
//!
//! Lookup order:
//!
//! 1. `--engine-config <path>` CLI flag (future), or third positional
//!    argument today;
//! 2. `engine.toml` in the current working directory;
//! 3. defaults - no chains configured, `state_dir = ./data`.
//!
//! A missing config is OK for the example module (it only logs); for
//! the cow-api / chain backends it surfaces as `HostError {
//! kind: unsupported }` so guests learn early.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use strum::IntoStaticStr;
use thiserror::Error;
use tracing::{info, warn};

/// Errors surfaced by [`load_or_default`].
///
/// Library-side modules must not propagate `anyhow::Error`; the rust
/// idiomatic rubric reserves `anyhow` for `main.rs` and
/// `supervisor.rs` top-level dispatch. The variants carry the
/// upstream error via `#[from]` so the caller in `main.rs` (which
/// uses `anyhow`) gets a free conversion through `?`.
///
/// `IntoStaticStr` exposes the snake_case variant name for metric
/// labels and structured-log `error_kind` fields.
#[derive(Debug, Error, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum EngineConfigError {
    /// Failed to read the config file from disk.
    #[error("read engine config: {0}")]
    Io(#[from] std::io::Error),
    /// Config file was unparseable as TOML.
    #[error("parse engine config: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Engine-side configuration loaded from `engine.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub engine: EngineSection,
    /// Per-module wasmtime resource limits. Applies uniformly to every
    /// module; per-module overrides land in 0.3.
    #[serde(default)]
    pub limits: ModuleLimits,
    /// Per-chain RPC URLs keyed by EVM chain id (decimal in TOML).
    /// Used by the `chain::request` host call and as the alloy provider
    /// pool seed.
    #[serde(default)]
    pub chains: BTreeMap<u64, ChainConfig>,
    /// Modules the supervisor should boot. Each entry resolves a
    /// `(component.wasm, module.toml)` pair on the local filesystem
    /// for 0.2 - content-addressed resolution (Swarm / OCI /
    /// `[[content.sources]]`) lands in 0.3 per
    /// `docs/03-module-discovery.md`.
    #[serde(default)]
    pub modules: Vec<ModuleEntry>,
}

/// One `[[modules]]` table from `engine.toml`.
///
/// Both fields are filesystem paths in 0.2. `manifest` defaults to
/// `module.toml` next to `path` if omitted, matching the bundle layout
/// in `docs/02-modules-events-packaging.md`.
#[derive(Debug, Deserialize)]
pub struct ModuleEntry {
    /// Path to the compiled `.wasm` component.
    pub path: std::path::PathBuf,
    /// Path to the module's `module.toml`. Defaults to `<path-parent>/module.toml`.
    #[serde(default)]
    pub manifest: Option<std::path::PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct EngineSection {
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// `tracing_subscriber::EnvFilter`-compatible directive. Defaults to
    /// `info` when absent; `RUST_LOG` overrides at process start.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Prometheus metrics exporter wiring (COW-1034). Absent table =
    /// disabled (the engine still installs the recorder so call sites
    /// stay live but no HTTP listener binds).
    #[serde(default)]
    pub metrics: MetricsSection,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            log_level: default_log_level(),
            metrics: MetricsSection::default(),
        }
    }
}

/// `[engine.metrics]` config. When `enabled = true` the engine starts
/// a Prometheus HTTP exporter on `bind_addr` and serves `/metrics`.
///
/// Default: disabled. Operators opt in explicitly so the M3 / M4
/// runbook smoke runs do not bind a port unintentionally.
#[derive(Debug, Deserialize)]
pub struct MetricsSection {
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

#[derive(Debug, Deserialize)]
pub struct ChainConfig {
    /// JSON-RPC endpoint. `ws://` and `wss://` engage alloy's pubsub
    /// transport (required for `eth_subscribe`); `http://` and `https://`
    /// engage the HTTP transport (request/response only).
    pub rpc_url: String,
    /// Optional CoW orderbook base URL override for this chain. When
    /// absent (the common case), the host uses the canonical
    /// `api.cow.fi/{slug}/api/v1` URL from `cowprotocol::Chain`. Set
    /// this to point at a staging/barn instance or a local mock (e.g.
    /// `tools/orderbook-mock` for the COW-1079 load test).
    #[serde(default)]
    pub orderbook_url: Option<String>,
}

/// Default fuel budget per `on_event` invocation (~1 billion WASM
/// instructions).
const DEFAULT_FUEL_PER_EVENT: u64 = 1_000_000_000;

/// Default linear-memory cap per module store (64 MiB).
const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Per-module wasmtime resource limits. Both fields are optional;
/// omitted values resolve to built-in defaults.
///
/// ```toml
/// [limits]
/// fuel_per_event = 1_000_000_000
/// memory_bytes   = 67_108_864
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct ModuleLimits {
    /// Fuel budget granted per `on_event` invocation.
    pub fuel_per_event: Option<u64>,
    /// Linear-memory cap in bytes per module store.
    pub memory_bytes: Option<usize>,
}

impl ModuleLimits {
    /// Resolved fuel budget (override or default).
    pub fn fuel(&self) -> u64 {
        self.fuel_per_event.unwrap_or(DEFAULT_FUEL_PER_EVENT)
    }

    /// Resolved memory cap (override or default).
    pub fn memory(&self) -> usize {
        self.memory_bytes.unwrap_or(DEFAULT_MEMORY_LIMIT)
    }
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
             chain::request and cow_api::submit_order will return Unsupported)"
        );
        return Ok(EngineConfig::default());
    }

    let raw = std::fs::read_to_string(&path)?;
    let cfg: EngineConfig = toml::from_str(&raw)?;
    info!(
        path = %path.display(),
        chains = cfg.chains.len(),
        state_dir = %cfg.engine.state_dir.display(),
        "engine config loaded",
    );
    Ok(cfg)
}
