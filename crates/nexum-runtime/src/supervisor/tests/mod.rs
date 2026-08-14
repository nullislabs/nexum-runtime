//! Shared fixtures and boot helpers for the supervisor test areas.

mod boot_refusals;
mod chain_gate;
mod cursors;
mod digest;
mod dispatch;
mod e2e;
mod ledger;
mod lifecycle;

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_chains::Chain;
use tracing_core::Level;

use super::admission::enforce_extension_sections;
use super::artifact::read_verified_component;
use super::cursors::{
    chainlog_cursor_key, commit_chain_log_cursor, progress_key, read_chain_log_cursor,
};
use super::dispatch::with_dispatch_deadline;
use super::prepass::{NamespaceLedger, claim_namespace, unconfigured_chain};
use super::store::resolve_module_limits;
use super::subscriptions::build_alloy_filter;
use super::*;
use crate::bindings::nexum;
use crate::digest::{ContentDigest, DigestMismatch};
use crate::engine_config::{ModuleLimits, ResolvedModuleLimits};
use crate::host::logs::LogSource;
use crate::host::provider_pool::ProviderPool;
use crate::manifest::{self, CapabilityError, CapabilityRegistry, ParseError, ResourceSection};
use crate::preset::CoreRuntime;
use crate::test_utils::{
    BootScenario, Entry, ManifestSource, Refusal, TestManifest, example_wasm_or_skip,
    mock_components, module_wasm_or_skip, test_wasmtime_engine,
};

type DefaultSupervisor = Supervisor<CoreRuntime>;

const SEPOLIA: u64 = 11_155_111;

/// Path to a manifest checked into the workspace tree.
fn workspace_manifest(relative: &str) -> PathBuf {
    crate::test_utils::wasm::workspace_root().join(relative)
}

fn core_extensions() -> Vec<Arc<dyn crate::host::extension::Extension<CoreRuntime>>> {
    Vec::new()
}

fn make_linker(engine: &wasmtime::Engine) -> Linker<HostState<CoreRuntime>> {
    crate::supervisor::build_linker::<CoreRuntime>(engine, &core_extensions())
        .expect("build_linker")
}

/// An empty chain pool and the given store.
fn test_components(store: crate::host::local_store_redb::LocalStore) -> Components<CoreRuntime> {
    Components {
        chain: ProviderPool::empty(),
        store,
        logs: crate::test_utils::in_memory_logs(),
    }
}

fn test_chains() -> ConfiguredChains {
    ConfiguredChains::from_config(&EngineConfig {
        chains: crate::test_utils::test_chain_configs(),
        ..EngineConfig::default()
    })
}

/// The caller-held `TempDir` cleans up the store on drop.
fn temp_local_store() -> (tempfile::TempDir, crate::host::local_store_redb::LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.redb");
    let store = crate::host::local_store_redb::LocalStore::open(path).expect("local store");
    (dir, store)
}

fn block_on(chain_id: u64) -> nexum::host::types::Block {
    nexum::host::types::Block {
        chain_id,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    }
}

/// Scenario terminals cover the multi-entry `boot` path; the returned
/// `TempDir` keeps the store alive.
async fn try_boot_single(
    wasm: &Path,
    manifest: Option<&Path>,
    require_digest: bool,
    clocks: Option<WasiClockOverride>,
) -> (tempfile::TempDir, anyhow::Result<DefaultSupervisor>) {
    let engine = test_wasmtime_engine();
    let linker = make_linker(&engine);
    let (dir, store) = temp_local_store();
    let entry = ModuleEntry {
        path: wasm.to_path_buf(),
        manifest: manifest.map(Path::to_path_buf),
    };
    let limits = ResolvedModuleLimits::default();
    let env = BootEnv {
        limits: &limits,
        configured_chains: test_chains(),
        require_component_digest: require_digest,
    };
    let result = Supervisor::boot_single(
        &engine,
        &linker,
        &entry,
        &test_components(store),
        &env,
        &core_extensions(),
        clocks,
    )
    .await
    .map_err(anyhow::Error::from);
    (dir, result)
}
