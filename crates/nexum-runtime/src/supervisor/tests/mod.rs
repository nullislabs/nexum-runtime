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

/// Workspace root: the topmost ancestor with a `Cargo.toml`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .filter(|d| d.join("Cargo.toml").is_file())
        .last()
        .unwrap_or(manifest)
        .to_path_buf()
}

/// Path to the pre-built example WASM component.
fn example_wasm() -> PathBuf {
    workspace_root().join("target/wasm32-wasip2/release/example.wasm")
}

fn example_module_toml() -> PathBuf {
    workspace_root().join("modules/example/module.toml")
}

/// Returns `None` and prints a skip message if the fixture isn't built.
fn example_wasm_or_skip() -> Option<PathBuf> {
    let p = example_wasm();
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "SKIP: {} not found - run `just build-module` to enable E2E tests",
            p.display()
        );
        None
    }
}

pub(crate) fn make_wasmtime_engine() -> wasmtime::Engine {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    wasmtime::Engine::new(&config).expect("wasmtime engine")
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

/// Boot a zero-module supervisor over the in-process mock backends via the
/// real `boot` path.
pub(crate) async fn boot_mock_supervisor(
    engine: &wasmtime::Engine,
) -> Supervisor<crate::test_utils::MockTypes> {
    let components = crate::test_utils::mock_components();
    let config = EngineConfig::default();
    let linker = crate::supervisor::build_linker::<crate::test_utils::MockTypes>(engine, &[])
        .expect("build_linker");
    Supervisor::boot(engine, &linker, &config, &components, &[], None)
        .await
        .expect("boot mock supervisor")
}

const SEPOLIA: u64 = 11_155_111;

/// A production module's built `.wasm`; hyphens in the name become underscores.
fn module_wasm(module_name: &str) -> PathBuf {
    let artifact = module_name.replace('-', "_");
    workspace_root().join(format!("target/wasm32-wasip2/release/{artifact}.wasm"))
}

fn module_wasm_or_skip(module_name: &str) -> Option<PathBuf> {
    let p = module_wasm(module_name);
    if p.exists() {
        Some(p)
    } else if std::env::var_os("CI").is_some() {
        // The CI test job builds every module wasm before running the
        // suite, so a missing artifact here means the pipeline regressed.
        // Fail loudly rather than skip into a hollow green.
        panic!(
            "{} not found under CI - the test job must build the module wasms before the suite runs",
            p.display()
        );
    } else {
        eprintln!(
            "SKIP: {} not found - build with `cargo build -p {module_name} --target wasm32-wasip2 --release`",
            p.display()
        );
        None
    }
}

/// Resolve the real `module.toml` for one of the production modules.
fn production_module_toml(relative_path: &str) -> PathBuf {
    workspace_root().join(relative_path)
}

fn synthetic_sepolia_block() -> nexum::host::types::Block {
    nexum::host::types::Block {
        chain_id: SEPOLIA,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    }
}

/// Boot a single module from `(wasm, manifest)` and return the live
/// supervisor.
async fn boot_production_module(
    engine: &wasmtime::Engine,
    linker: &Linker<HostState<TestTypes>>,
    local_store: &crate::host::local_store_redb::LocalStore,
    wasm: &Path,
    manifest: &Path,
) -> DefaultSupervisor {
    let components = test_components(local_store.clone());
    let limits = ModuleLimits::default();
    Supervisor::boot_single(
        engine,
        linker,
        wasm,
        Some(manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot_single")
}

fn fixture_module_toml(relative_path: &str) -> PathBuf {
    workspace_root().join(relative_path)
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
