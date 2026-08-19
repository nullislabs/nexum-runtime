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
use super::artifact::{DigestPolicy, read_verified_component};
use super::cursors::{
    chainlog_cursor_key, commit_chain_log_cursor, progress_key, read_chain_log_cursor,
};
use super::dispatch::with_dispatch_deadline;
use super::prepass::{
    NamespaceLedger, claim_namespace, enforce_total_reservation, unconfigured_chain,
};
use super::sources::{build_alloy_filter, wit_log};
use super::store::resolve_module_limits;
use super::*;
use crate::bindings::nexum;
use crate::engine_config::{
    ComponentPolicy, PolicyCeilings, PolicySection, ResolvedModuleLimits, TotalPolicy,
};
use crate::error::RuntimeError;
use crate::manifest::error::CapabilityError;
use crate::manifest::{self, CapabilityRegistry, ParseError, ResourceSection};
use crate::preset::CoreRuntime;
use crate::supervisor::load::LoadRefusal;
use crate::supervisor::prepass::BootRefusal;
use crate::test_utils::{
    BootScenario, Entry, ManifestInput, Refusal, TestManifest, example_wasm_or_skip, limits_with,
    mock_components, module_wasm_or_skip, test_wasmtime_engine,
};
use nexum_primitives::digest::{ContentDigest, DigestMismatch};
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_logs::LogChannel;

type DefaultSupervisor = Supervisor<CoreRuntime>;

const SEPOLIA: u64 = 11_155_111;

/// Path to a manifest checked into the workspace tree.
fn workspace_manifest(relative: &str) -> PathBuf {
    crate::test_utils::wasm::workspace_root().join(relative)
}

fn core_extensions() -> Vec<Arc<dyn nexum_runtime_api::Extension<CoreRuntime>>> {
    Vec::new()
}

fn make_linker(engine: &wasmtime::Engine) -> Linker<HostState<CoreRuntime>> {
    crate::supervisor::build_linker::<CoreRuntime>(engine, &core_extensions())
        .expect("build_linker")
}

/// An empty chain pool and the given store.
fn test_components(store: nexum_runtime_store::LocalStore) -> Components<CoreRuntime> {
    Components {
        chain: ProviderPool::empty(),
        store,
        logs: crate::test_utils::in_memory_logs(),
    }
}

fn test_chains() -> ConfiguredChains {
    let mut config = EngineConfig::default();
    config.chains = crate::test_utils::test_chain_configs();
    ConfiguredChains::from_config(&config)
}

/// The caller-held `TempDir` cleans up the store on drop.
fn temp_local_store() -> (tempfile::TempDir, nexum_runtime_store::LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.redb");
    let store = nexum_runtime_store::LocalStore::open(path).expect("local store");
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
) -> (tempfile::TempDir, Result<DefaultSupervisor, RuntimeError>) {
    let engine = test_wasmtime_engine();
    let linker = make_linker(&engine);
    let (dir, store) = temp_local_store();
    let mut entry = ModuleEntry::new("single", wasm);
    entry.manifest = manifest.map(Path::to_path_buf);
    let limits = ResolvedModuleLimits::default();
    let policy = PolicySection::default();
    let env = BootEnv {
        limits: &limits,
        policy: &policy,
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
    .await;
    (dir, result)
}
