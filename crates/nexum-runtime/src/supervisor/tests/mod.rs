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

use super::*;
use crate::engine_config::ModuleLimits;
use crate::manifest::ResourceSection;
use crate::test_utils::{
    BootScenario, Entry, ManifestSource, Refusal, TestManifest, example_wasm_or_skip,
    mock_components, module_wasm_or_skip, test_wasmtime_engine,
};

const SEPOLIA: u64 = 11_155_111;

/// Path to a manifest checked into the workspace tree.
fn workspace_manifest(relative: &str) -> PathBuf {
    crate::test_utils::wasm::workspace_root().join(relative)
}

/// The core-only extension set: no domain extensions.
fn core_extensions() -> Vec<Arc<dyn crate::host::extension::Extension<TestTypes>>> {
    Vec::new()
}

fn make_linker(engine: &wasmtime::Engine) -> Linker<HostState<TestTypes>> {
    crate::supervisor::build_linker::<TestTypes>(engine, &core_extensions()).expect("build_linker")
}

/// Synthetic component bundle for tests: an empty chain pool, an empty
/// extension slot, and the given store.
fn test_components(store: crate::host::local_store_redb::LocalStore) -> Components<TestTypes> {
    Components {
        chain: ProviderPool::empty(),
        store,
        ext: (),
        logs: crate::test_utils::in_memory_logs(),
    }
}

/// [`ConfiguredChains`] over the shared test chain set.
fn test_chains() -> ConfiguredChains {
    ConfiguredChains::from_config(&EngineConfig {
        chains: crate::test_utils::test_chain_configs(),
        ..EngineConfig::default()
    })
}

/// Return `(dir, store)` so the test holds the `TempDir` and cleans it up
/// on drop.
fn temp_local_store() -> (tempfile::TempDir, crate::host::local_store_redb::LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.redb");
    let store = crate::host::local_store_redb::LocalStore::open(path).expect("local store");
    (dir, store)
}

/// A synthetic block on `chain_id` for direct dispatch calls.
fn block_on(chain_id: u64) -> nexum::host::types::Block {
    nexum::host::types::Block {
        chain_id,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    }
}

/// Drive the `boot_single` entry point directly; scenario terminals cover
/// the multi-entry `boot` path. The returned `TempDir` keeps the store alive.
async fn try_boot_single(
    wasm: &Path,
    manifest: Option<&Path>,
    require_digest: bool,
    clocks: Option<WasiClockOverride>,
) -> (tempfile::TempDir, anyhow::Result<DefaultSupervisor>) {
    let engine = test_wasmtime_engine();
    let linker = make_linker(&engine);
    let (dir, store) = temp_local_store();
    let result = Supervisor::boot_single(
        &engine,
        &linker,
        wasm,
        manifest,
        &test_components(store),
        &ModuleLimits::default(),
        &test_chains(),
        require_digest,
        &core_extensions(),
        clocks,
    )
    .await;
    (dir, result)
}

/// A stub extension registering the `acme-adapter` provider kind behind a
/// unit service, for the boot-gate tests.
struct AcmeService;
impl crate::host::extension::HostService for AcmeService {}

struct AcmeKind;

#[async_trait::async_trait]
impl ProviderKind<crate::test_utils::MockTypes> for AcmeKind {
    fn kind(&self) -> &'static str {
        "acme-adapter"
    }

    fn link(
        &self,
        _linker: &mut Linker<HostState<crate::test_utils::MockTypes>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn install(
        &self,
        _instance: ProviderInstance<'_, crate::test_utils::MockTypes>,
        _service: &Arc<dyn HostService>,
    ) -> anyhow::Result<Installed> {
        Ok(Installed::Live)
    }
}

struct AcmeExtension;

impl Extension<crate::test_utils::MockTypes> for AcmeExtension {
    fn namespace(&self) -> &'static str {
        "acme"
    }

    fn capabilities(&self) -> manifest::NamespaceCaps {
        manifest::NamespaceCaps {
            prefix: "test:acme/",
            ifaces: &[],
        }
    }

    fn link(
        &self,
        _linker: &mut Linker<HostState<crate::test_utils::MockTypes>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn service(&self) -> Option<Arc<dyn HostService>> {
        Some(Arc::new(AcmeService))
    }

    fn provider(&self) -> Option<Box<dyn ProviderKind<crate::test_utils::MockTypes>>> {
        Some(Box::new(AcmeKind))
    }
}

/// The stub extension set registering the `acme-adapter` kind.
fn acme_extensions() -> Vec<Arc<dyn Extension<crate::test_utils::MockTypes>>> {
    vec![Arc::new(AcmeExtension)]
}
