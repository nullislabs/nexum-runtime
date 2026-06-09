//! Engine-side runtime configuration.
//!
//! Distinct from `module.toml` (module manifest): this file describes
//! the *engine*'s I/O wiring — chain RPC endpoints and the on-disk
//! location of the `local-store` database. Both are required for the
//! 0.2 reference engine to do anything other than print stubs.
//!
//! Lookup order:
//!
//! 1. `--engine-config <path>` CLI flag (future), or third positional
//!    argument today;
//! 2. `engine.toml` in the current working directory;
//! 3. defaults — no chains configured, `state_dir = ./data`.
//!
//! A missing config is OK for the example module (it only logs); for
//! the cow-api / chain backends it surfaces as `HostError {
//! kind: unsupported }` so guests learn early.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{info, warn};

/// Engine-side configuration loaded from `engine.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub engine: EngineSection,
    /// Per-chain RPC URLs keyed by EVM chain id (decimal in TOML).
    /// Used by the `chain::request` host call and as the alloy provider
    /// pool seed.
    #[serde(default)]
    pub chains: BTreeMap<u64, ChainConfig>,
    /// Modules the supervisor should boot. Each entry resolves a
    /// `(component.wasm, module.toml)` pair on the local filesystem
    /// for 0.2 — content-addressed resolution (Swarm / OCI /
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
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            log_level: default_log_level(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChainConfig {
    /// JSON-RPC endpoint. `ws://` and `wss://` engage alloy's pubsub
    /// transport (required for `eth_subscribe`); `http://` and `https://`
    /// engage the HTTP transport (request/response only).
    pub rpc_url: String,
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_log_level() -> String {
    "info".to_owned()
}

/// Read an engine config from disk, returning defaults if the file is
/// missing. Parse errors propagate.
pub fn load_or_default(path: Option<&Path>) -> anyhow::Result<EngineConfig> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("engine.toml"),
    };

    if !path.exists() {
        warn!(
            path = %path.display(),
            "engine.toml not found — running with defaults (no chain RPC endpoints; \
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
