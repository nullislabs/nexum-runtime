//! One-expression supervisor boot: manifests in a tempdir, an engine
//! config over them, and the real [`Supervisor::boot`] admission path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alloy_chains::Chain;
use tempfile::TempDir;

use super::manifest::TestManifest;
use super::{in_memory_logs, test_chain_configs};
use crate::engine_config::{AdapterEntry, ChainConfig, EngineConfig, ModuleEntry};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::local_store_redb::LocalStore;
use crate::host::provider_pool::ProviderPool;
use crate::supervisor::{Supervisor, WasiClockOverride, build_linker};
use crate::test_utils::wasm::test_wasmtime_engine;

/// Core-only lattice over the on-disk redb store, the scenario default;
/// the chain leg stays an empty [`ProviderPool`], so no transport exists.
pub struct RedbTypes;

impl crate::sealed::SealedRuntimeTypes for RedbTypes {}

impl RuntimeTypes for RedbTypes {
    type Store = LocalStore;
    type Ext = ();
}

/// Builder collapsing the tempdir + write-manifests + engine-config + boot
/// ritual; every terminal goes through the real [`Supervisor::boot`] path.
pub struct BootScenario {
    wasm: Option<PathBuf>,
    modules: Vec<TestManifest>,
    adapters: Vec<TestManifest>,
    chains: HashMap<Chain, ChainConfig>,
    state_dir: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
}

impl BootScenario {
    /// An empty scenario; `[chains]` defaults to [`test_chain_configs`].
    pub fn new() -> Self {
        Self {
            wasm: None,
            modules: Vec::new(),
            adapters: Vec::new(),
            chains: test_chain_configs(),
            state_dir: None,
            clocks: None,
        }
    }

    /// The component every module and adapter entry loads; unset, entries
    /// point at a nonexistent path, which only pre-compile refusals survive.
    pub fn wasm(mut self, wasm: impl Into<PathBuf>) -> Self {
        self.wasm = Some(wasm.into());
        self
    }

    /// Append a `[[modules]]` entry booting `manifest`.
    pub fn module(mut self, manifest: TestManifest) -> Self {
        self.modules.push(manifest);
        self
    }

    /// Append an `[[adapters]]` entry booting `manifest` with an empty
    /// operator transport grant.
    pub fn adapter(mut self, manifest: TestManifest) -> Self {
        self.adapters.push(manifest);
        self
    }

    /// Replace the `[chains]` table the boot-time gates read.
    pub fn chains(mut self, chains: HashMap<Chain, ChainConfig>) -> Self {
        self.chains = chains;
        self
    }

    /// Root the engine state (and the scenario's redb store) at `dir`
    /// instead of the scenario tempdir.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Per-store WASI clock override threaded to boot, mirroring
    /// [`Supervisor::boot_single`]; `None` keeps the ambient host clocks.
    pub fn clock(mut self, clocks: Option<WasiClockOverride>) -> Self {
        self.clocks = Some(clocks).flatten();
        self
    }

    /// Boot through [`Supervisor::boot`] over an empty provider pool and a
    /// fresh redb store under the state dir.
    pub async fn boot(self) -> anyhow::Result<Booted> {
        let tmp = tempfile::tempdir()?;
        let (config, clocks) = self.prepare(tmp.path())?;
        std::fs::create_dir_all(&config.engine.state_dir)?;
        let store = LocalStore::open(config.engine.state_dir.join("scenario.redb"))?;
        let components = Components::<RedbTypes> {
            chain: ProviderPool::empty(),
            store,
            ext: (),
            logs: in_memory_logs(),
        };
        let engine = test_wasmtime_engine();
        let linker = build_linker::<RedbTypes>(&engine, &[])?;
        let supervisor =
            Supervisor::boot(&engine, &linker, &config, &components, &[], clocks).await?;
        Ok(Booted {
            supervisor,
            _tmp: tmp,
        })
    }

    /// Boot and demand a refusal, panicking if the supervisor came up.
    pub async fn expect_refusal(self) -> Refusal {
        match self.boot().await {
            Ok(_) => panic!("boot succeeded where a refusal was expected"),
            Err(err) => Refusal(err),
        }
    }

    /// Write every manifest under `dir` and assemble the engine config;
    /// the returned clocks ride separately, `Supervisor::boot` takes them
    /// beside the config.
    fn prepare(self, dir: &Path) -> anyhow::Result<(EngineConfig, Option<WasiClockOverride>)> {
        let wasm = self.wasm.unwrap_or_else(|| dir.join("component.wasm"));
        let mut config = EngineConfig::default();
        config.engine.state_dir = self.state_dir.unwrap_or_else(|| dir.join("state"));
        config.chains = self.chains;
        for (i, manifest) in self.modules.iter().enumerate() {
            let path = manifest.write_as(&dir.join(format!("module-{i}.toml")));
            config.modules.push(ModuleEntry {
                path: wasm.clone(),
                manifest: Some(path),
            });
        }
        for (i, manifest) in self.adapters.iter().enumerate() {
            let path = manifest.write_as(&dir.join(format!("adapter-{i}.toml")));
            config.adapters.push(AdapterEntry {
                path: wasm.clone(),
                manifest: Some(path),
                http_allow: Vec::new(),
                messaging_topics: Vec::new(),
            });
        }
        Ok((config, self.clocks))
    }
}

impl Default for BootScenario {
    fn default() -> Self {
        Self::new()
    }
}

/// A booted supervisor plus the tempdir rooting its manifests and store;
/// drop order keeps the store's backing file alive with the supervisor.
pub struct Booted {
    pub supervisor: Supervisor<RedbTypes>,
    _tmp: TempDir,
}

/// A boot refusal; assertions read the rendered context chain, so one
/// helper is the choke point for every string-contains check.
#[derive(Debug)]
pub struct Refusal(anyhow::Error);

impl Refusal {
    /// Assert the refusal names `needle` somewhere in its context chain.
    #[track_caller]
    pub fn names(self, needle: &str) -> Self {
        let chain = self.chain();
        assert!(
            chain.contains(needle),
            "refusal does not name {needle:?}: {chain}",
        );
        self
    }

    /// Assert the refusal never mentions `needle` in its context chain.
    #[track_caller]
    pub fn lacks(self, needle: &str) -> Self {
        let chain = self.chain();
        assert!(!chain.contains(needle), "refusal names {needle:?}: {chain}");
        self
    }

    /// The rendered context chain, outermost cause first.
    fn chain(&self) -> String {
        format!("{:#}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::example_wasm_or_skip;

    #[tokio::test]
    async fn a_scenario_module_boots_alive_and_takes_dispatch() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let mut booted = BootScenario::new()
            .wasm(&wasm)
            .module(TestManifest::new("example").cap("logging").block_sub(1))
            .boot()
            .await
            .expect("scenario boot");

        assert_eq!(booted.supervisor.module_count(), 1);
        assert_eq!(booted.supervisor.alive_count(), 1);
        let block = crate::bindings::nexum::host::types::Block {
            chain_id: 1,
            number: 19_000_000,
            hash: vec![0xab; 32],
            timestamp: 1_700_000_000_000,
        };
        assert_eq!(booted.supervisor.dispatch_block(block).await, 1);
        assert_eq!(booted.supervisor.alive_count(), 1);
    }

    #[tokio::test]
    async fn a_manual_clock_override_rides_the_scenario_boot() {
        use std::time::{Duration, UNIX_EPOCH};

        use crate::test_utils::clock::ManualClock;

        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let clock = ManualClock::new();
        clock.set(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let booted = BootScenario::new()
            .wasm(&wasm)
            .module(TestManifest::new("example").cap("logging"))
            .clock(Some(clock.as_override()))
            .boot()
            .await
            .expect("scenario boot with a clock override");
        assert_eq!(booted.supervisor.alive_count(), 1);
    }

    #[tokio::test]
    async fn an_unknown_module_capability_refuses_before_any_compile() {
        BootScenario::new()
            .module(TestManifest::new("bad").cap("telepathy"))
            .expect_refusal()
            .await
            .names("unknown capability")
            .names("telepathy")
            .lacks("compile");
    }

    #[tokio::test]
    async fn an_unregistered_adapter_kind_refuses_before_any_compile() {
        BootScenario::new()
            .adapter(TestManifest::new("feed").kind("acme-feed"))
            .expect_refusal()
            .await
            .names("unregistered provider kind acme-feed")
            .lacks("compile");
    }

    #[tokio::test]
    #[should_panic(expected = "refusal was expected")]
    async fn expect_refusal_panics_when_boot_succeeds() {
        BootScenario::new().expect_refusal().await;
    }

    #[tokio::test]
    async fn state_dir_hosts_the_scenario_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state-here");
        let booted = BootScenario::new()
            .state_dir(&state)
            .boot()
            .await
            .expect("an empty scenario boots");
        assert_eq!(booted.supervisor.module_count(), 0);
        assert!(state.join("scenario.redb").is_file());
    }

    #[test]
    fn prepare_pairs_every_manifest_with_the_scenario_wasm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, clocks) = BootScenario::new()
            .wasm("guest.wasm")
            .module(TestManifest::new("a").cap("logging"))
            .module(TestManifest::new("b").cap("logging"))
            .adapter(TestManifest::new("feed").kind("acme-feed"))
            .chains(HashMap::new())
            .prepare(dir.path())
            .expect("prepare");

        assert!(clocks.is_none());
        assert!(
            config.chains.is_empty(),
            "the chains knob replaces the default"
        );
        assert_eq!(config.engine.state_dir, dir.path().join("state"));
        assert_eq!(config.modules.len(), 2);
        assert_eq!(config.adapters.len(), 1);
        for (entry_path, manifest) in config
            .modules
            .iter()
            .map(|m| (&m.path, &m.manifest))
            .chain(config.adapters.iter().map(|a| (&a.path, &a.manifest)))
        {
            assert_eq!(entry_path, Path::new("guest.wasm"));
            assert!(
                manifest.as_deref().is_some_and(Path::is_file),
                "manifest written: {manifest:?}",
            );
        }
    }

    #[test]
    fn default_chains_cover_the_fixture_chain_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _) = BootScenario::new().prepare(dir.path()).expect("prepare");
        for id in [1, 100, 11_155_111] {
            assert!(
                config.chains.contains_key(&Chain::from_id(id)),
                "chain {id} configured",
            );
        }
    }

    #[test]
    fn names_and_lacks_read_the_whole_context_chain() {
        let refusal = Refusal(anyhow::anyhow!("root cause").context("outer context"));
        refusal
            .names("outer context")
            .names("root cause")
            .lacks("absent needle");
    }

    #[test]
    #[should_panic(expected = "refusal does not name")]
    fn names_panics_on_an_absent_needle() {
        Refusal(anyhow::anyhow!("boom")).names("quiet");
    }

    #[test]
    #[should_panic(expected = "refusal names")]
    fn lacks_panics_on_a_present_needle() {
        Refusal(anyhow::anyhow!("boom")).lacks("boom");
    }
}
