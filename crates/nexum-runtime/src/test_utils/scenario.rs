//! One-expression supervisor boot through the real [`Supervisor::boot`] path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_chains::Chain;
use derive_more::From;
use tempfile::TempDir;

use super::manifest::{ManifestSource, TestManifest};
use super::{in_memory_logs, test_chain_configs};
use crate::engine_config::{AdapterEntry, ChainConfig, EngineConfig, ModuleEntry, ModuleLimits};
use crate::host::component::{Components, RuntimeTypes};
use crate::host::extension::{Extension, attach_wall_clock};
use crate::host::local_store_redb::LocalStore;
use crate::host::logs::{LogPipeline, LogRecord};
use crate::host::provider_pool::ProviderPool;
use crate::preset::CoreRuntime;
use crate::supervisor::{Supervisor, WasiClockOverride, build_linker};
use crate::test_utils::wasm::test_wasmtime_engine;

/// One `[[modules]]` or `[[adapters]]` entry.
pub struct Entry {
    wasm: Option<PathBuf>,
    manifest: ManifestSource,
    http_allow: Vec<String>,
}

impl Entry {
    /// An entry loading `manifest` on the scenario-wide component.
    pub fn new(manifest: impl Into<ManifestSource>) -> Self {
        Self {
            wasm: None,
            manifest: manifest.into(),
            http_allow: Vec::new(),
        }
    }

    /// Load this entry from `wasm` rather than the scenario-wide component.
    pub fn wasm(mut self, wasm: impl Into<PathBuf>) -> Self {
        self.wasm = Some(wasm.into());
        self
    }

    /// Operator HTTP grant; only an `[[adapters]]` entry carries one.
    pub fn http_allow(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.http_allow.extend(hosts.into_iter().map(Into::into));
        self
    }
}

impl From<TestManifest> for Entry {
    fn from(manifest: TestManifest) -> Self {
        Self::new(manifest)
    }
}

impl From<String> for Entry {
    fn from(toml: String) -> Self {
        Self::new(toml)
    }
}

impl From<PathBuf> for Entry {
    fn from(manifest: PathBuf) -> Self {
        Self::new(manifest)
    }
}

/// Every terminal boots through the real [`Supervisor::boot`] admission path.
pub struct BootScenario<T: RuntimeTypes = CoreRuntime> {
    dir: TempDir,
    components: Components<T>,
    extensions: Vec<Arc<dyn Extension<T>>>,
    limits: ModuleLimits,
    chains: HashMap<Chain, ChainConfig>,
    wasm: Option<PathBuf>,
    modules: Vec<Entry>,
    adapters: Vec<Entry>,
    clocks: Option<WasiClockOverride>,
    require_digest: bool,
    defaulted: bool,
}

impl BootScenario<CoreRuntime> {
    /// A fresh redb store under the scenario directory and an empty provider pool.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("scenario tempdir");
        let store = LocalStore::open(dir.path().join("scenario.redb")).expect("scenario store");
        Self::rooted(
            dir,
            Components {
                chain: ProviderPool::empty(),
                store,
                logs: in_memory_logs(),
            },
        )
    }
}

impl<T: RuntimeTypes> BootScenario<T> {
    pub fn over(components: Components<T>) -> Self {
        Self::rooted(tempfile::tempdir().expect("scenario tempdir"), components)
    }

    fn rooted(dir: TempDir, components: Components<T>) -> Self {
        Self {
            dir,
            components,
            extensions: Vec::new(),
            limits: ModuleLimits::default(),
            chains: test_chain_configs(),
            wasm: None,
            modules: Vec::new(),
            adapters: Vec::new(),
            clocks: None,
            require_digest: false,
            defaulted: false,
        }
    }

    /// The directory inline manifests are written under.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Scenario-wide component; unset, entries point at a nonexistent path.
    pub fn wasm(mut self, wasm: impl Into<PathBuf>) -> Self {
        self.wasm = Some(wasm.into());
        self
    }

    pub fn module(mut self, entry: impl Into<Entry>) -> Self {
        self.modules.push(entry.into());
        self
    }

    pub fn adapter(mut self, entry: impl Into<Entry>) -> Self {
        self.adapters.push(entry.into());
        self
    }

    pub fn limits(mut self, limits: ModuleLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Operator-permitted destinations, as `[limits.http].permit_destinations`.
    /// A test serving over loopback needs this: the address rules refuse
    /// loopback by default and a module allowlist cannot widen them.
    pub fn permit_destinations(
        mut self,
        addrs: impl IntoIterator<Item = std::net::IpAddr>,
    ) -> Self {
        self.limits.http.permit_destinations = addrs.into_iter().collect();
        self
    }

    /// Replace the `[chains]` set; defaults to [`test_chain_configs`].
    pub fn chains(mut self, chains: HashMap<Chain, ChainConfig>) -> Self {
        self.chains = chains;
        self
    }

    /// Model a run without any engine.toml: no chains, defaulted config.
    pub fn defaulted_chains(mut self) -> Self {
        self.chains = HashMap::new();
        self.defaulted = true;
        self
    }

    /// Refuse any entry whose manifest lacks a component digest pin.
    pub fn require_digest(mut self) -> Self {
        self.require_digest = true;
        self
    }

    /// Wire extensions; they reach the linker and the boot gates together.
    pub fn extensions(
        mut self,
        extensions: impl IntoIterator<Item = Arc<dyn Extension<T>>>,
    ) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Unset keeps the ambient host clocks.
    pub fn clock(mut self, clocks: WasiClockOverride) -> Self {
        self.clocks = Some(clocks);
        self
    }

    pub async fn boot(self) -> anyhow::Result<Booted<T>> {
        let (config, launch) = self.split();
        let engine = test_wasmtime_engine();
        attach_wall_clock(&launch.extensions, launch.clocks.as_ref());
        let linker = build_linker::<T>(&engine, &launch.extensions)?;
        let supervisor = Supervisor::boot(
            &engine,
            &linker,
            &config,
            &launch.components,
            &launch.extensions,
            launch.clocks,
        )
        .await?;
        Ok(Booted {
            supervisor,
            logs: launch.components.logs,
            _dir: launch.dir,
        })
    }

    /// Boot and demand a refusal, panicking if the supervisor came up.
    pub async fn expect_refusal(self) -> Refusal {
        match self.boot().await {
            Ok(_) => panic!("boot succeeded where a refusal was expected"),
            Err(err) => Refusal(err),
        }
    }

    fn split(self) -> (EngineConfig, Launch<T>) {
        let dir = self.dir.path().to_path_buf();
        let default_wasm = self.wasm.unwrap_or_else(|| dir.join("component.wasm"));
        let resolve = |role: &str, i: usize, entry: Entry| {
            let at = dir.join(format!("{role}-{i}.toml"));
            (
                entry.wasm.unwrap_or_else(|| default_wasm.clone()),
                entry.manifest.resolve(&at),
                entry.http_allow,
            )
        };

        let mut config = EngineConfig {
            limits: self.limits,
            chains: self.chains,
            ..Default::default()
        };
        config.engine.state_dir = dir.clone();
        config.engine.require_component_digest = self.require_digest;
        config.defaulted = self.defaulted;
        for (i, entry) in self.modules.into_iter().enumerate() {
            let (path, manifest, ..) = resolve("module", i, entry);
            config.modules.push(ModuleEntry { path, manifest });
        }
        for (i, entry) in self.adapters.into_iter().enumerate() {
            let (path, manifest, http_allow) = resolve("adapter", i, entry);
            config.adapters.push(AdapterEntry {
                path,
                manifest,
                http_allow,
            });
        }
        (
            config,
            Launch {
                dir: self.dir,
                components: self.components,
                extensions: self.extensions,
                clocks: self.clocks,
            },
        )
    }
}

struct Launch<T: RuntimeTypes> {
    dir: TempDir,
    components: Components<T>,
    extensions: Vec<Arc<dyn Extension<T>>>,
    clocks: Option<WasiClockOverride>,
}

impl Default for BootScenario<CoreRuntime> {
    fn default() -> Self {
        Self::new()
    }
}

/// Booted supervisor; the held tempdir keeps its manifests and store alive.
pub struct Booted<T: RuntimeTypes = CoreRuntime> {
    pub supervisor: Supervisor<T>,
    logs: LogPipeline,
    _dir: TempDir,
}

impl<T: RuntimeTypes> Booted<T> {
    pub fn logs(&self) -> &LogPipeline {
        &self.logs
    }

    /// Dispatch a synthetic block on `chain_id`; returns the modules reached.
    pub async fn dispatch_block_on(&mut self, chain_id: u64) -> usize {
        self.supervisor
            .dispatch_block(crate::bindings::nexum::host::types::Block {
                chain_id,
                number: 19_000_000,
                hash: vec![0xab; 32],
                timestamp: 1_700_000_000_000,
            })
            .await
    }

    /// Every record `module` logged, across all of its runs.
    pub fn records(&self, module: &str) -> Vec<LogRecord> {
        self.logs
            .list_runs(module)
            .into_iter()
            .flat_map(|meta| self.logs.read(&meta.run, 0).records)
            .collect()
    }
}

#[derive(Debug, From)]
pub struct Refusal(anyhow::Error);

impl Refusal {
    /// The typed root under the context wraps, for `matches!` on a variant.
    pub fn root<E: std::error::Error + Send + Sync + 'static>(&self) -> Option<&E> {
        self.0.chain().find_map(|cause| cause.downcast_ref::<E>())
    }

    /// Assert the chain carries an `E` matching `pred`.
    #[track_caller]
    pub fn variant<E>(self, pred: impl FnOnce(&E) -> bool) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let Some(root) = self.root::<E>() else {
            panic!(
                "refusal carries no {}: {}",
                std::any::type_name::<E>(),
                self.chain(),
            )
        };
        assert!(pred(root), "refusal variant mismatch: {root:?}");
        self
    }

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
    use crate::host::extension::HostWallClock;
    use crate::manifest::{NamespaceCaps, ParseError};
    use crate::supervisor::load::LoadRefusal;
    use crate::supervisor::prepass::BootRefusal;
    use crate::test_utils::{example_wasm_or_skip, module_wasm_or_skip};

    /// Claims the `[acme]` manifest section and nothing else.
    struct AcmeExtension;

    impl Extension<CoreRuntime> for AcmeExtension {
        fn namespace(&self) -> &'static str {
            "acme"
        }

        fn capabilities(&self) -> NamespaceCaps {
            NamespaceCaps {
                prefix: "test:acme/",
                ifaces: &[],
            }
        }

        fn link(
            &self,
            _linker: &mut wasmtime::component::Linker<crate::host::state::HostState<CoreRuntime>>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn manifest_sections(&self) -> &'static [&'static str] {
            &["acme"]
        }
    }

    /// Records the wall clock the boot path hands the extension seam.
    struct ClockCaptureExtension(Arc<std::sync::OnceLock<Arc<dyn HostWallClock + Send + Sync>>>);

    impl Extension<CoreRuntime> for ClockCaptureExtension {
        fn namespace(&self) -> &'static str {
            "clockcap"
        }

        fn capabilities(&self) -> NamespaceCaps {
            NamespaceCaps {
                prefix: "test:clockcap/",
                ifaces: &[],
            }
        }

        fn link(
            &self,
            _linker: &mut wasmtime::component::Linker<crate::host::state::HostState<CoreRuntime>>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn attach_clock(&self, wall: Arc<dyn HostWallClock + Send + Sync>) {
            let _ = self.0.set(wall);
        }
    }

    /// A manifest declaring the `[acme]` section, which no builder emits.
    fn acme_section_manifest() -> String {
        "[module]\nname = \"acme-user\"\n\n[capabilities]\nrequired = []\n\n[acme]\nventure = 1\n"
            .to_owned()
    }

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
        assert_eq!(booted.dispatch_block_on(1).await, 1);
        assert_eq!(booted.supervisor.alive_count(), 1);
    }

    #[tokio::test]
    async fn per_entry_components_boot_alongside_the_scenario_default() {
        let Some(example) = example_wasm_or_skip() else {
            return;
        };
        let Some(reader) = module_wasm_or_skip("clock-reader") else {
            return;
        };
        let mut booted = BootScenario::new()
            .wasm(&example)
            .module(TestManifest::new("example").cap("logging").block_sub(1))
            .module(
                Entry::new(
                    TestManifest::new("clock-reader")
                        .cap("logging")
                        .block_sub(1),
                )
                .wasm(&reader),
            )
            .boot()
            .await
            .expect("scenario boot over two components");

        assert_eq!(booted.supervisor.module_count(), 2);
        assert_eq!(booted.supervisor.alive_count(), 2);
        assert_eq!(booted.dispatch_block_on(1).await, 2);
        assert!(
            booted
                .records("clock-reader")
                .iter()
                .any(|record| record.message.starts_with("clock wall")),
            "the per-entry component dispatched, not the scenario default",
        );
    }

    /// One boot, one override, both readers: the guest logs the pinned wall
    /// time and the extension seam reads the same instant.
    #[tokio::test]
    async fn a_clock_override_reaches_the_booted_guest_and_the_extension_seam() {
        use std::sync::OnceLock;
        use std::time::{Duration, UNIX_EPOCH};

        use crate::test_utils::clock::ManualClock;

        let Some(wasm) = module_wasm_or_skip("clock-reader") else {
            return;
        };
        // Far from the ambient clock; an exact match can only come from the override.
        const PINNED_SECS: u64 = 1_700_000_000;

        let clock = ManualClock::new();
        clock.set(UNIX_EPOCH + Duration::from_secs(PINNED_SECS));
        let seen = Arc::new(OnceLock::new());
        let capture: Arc<dyn Extension<CoreRuntime>> =
            Arc::new(ClockCaptureExtension(seen.clone()));
        let mut booted = BootScenario::new()
            .wasm(&wasm)
            .module(
                TestManifest::new("clock-reader")
                    .cap("logging")
                    .block_sub(1),
            )
            .extensions([capture])
            .clock(clock.as_override())
            .boot()
            .await
            .expect("scenario boot with a clock override");

        assert_eq!(booted.dispatch_block_on(1).await, 1);
        let logged: Vec<String> = booted
            .records("clock-reader")
            .into_iter()
            .map(|record| record.message)
            .collect();
        let pinned = format!("clock wall {PINNED_SECS}");
        assert!(
            logged.contains(&pinned),
            "the guest read the overridden wall clock: {logged:?}",
        );
        assert_eq!(
            seen.get().expect("boot attached a clock").now(),
            Duration::from_secs(PINNED_SECS),
            "the extension and the guest read one timeline",
        );
    }

    #[tokio::test]
    async fn an_unknown_module_capability_refuses_before_any_compile() {
        BootScenario::new()
            .module(TestManifest::new("bad").cap("telepathy"))
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::Manifest(ParseError::UnknownCapability { name, .. })
                    if name == "telepathy")
            })
            .lacks("compile");
    }

    #[tokio::test]
    async fn an_unregistered_adapter_kind_refuses_before_any_compile() {
        BootScenario::new()
            .adapter(TestManifest::new("feed").kind("acme-feed"))
            .expect_refusal()
            .await
            .variant::<LoadRefusal>(
                |e| matches!(e, LoadRefusal::UnregisteredKind { kind, .. } if kind == "acme-feed"),
            )
            .lacks("compile");
    }

    #[tokio::test]
    async fn a_component_without_any_manifest_refuses_on_discovery() {
        let scenario = BootScenario::new();
        let orphan = scenario.dir().join("orphan.wasm");
        scenario
            .module(Entry::new(ManifestSource::Beside).wasm(orphan))
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::ManifestMissing { component }
                    if component.ends_with("orphan.wasm"))
            })
            .lacks("compile");
    }

    #[tokio::test]
    async fn a_nonexistent_explicit_manifest_path_refuses() {
        let scenario = BootScenario::new();
        let missing = scenario.dir().join("modle.toml");
        scenario
            .module(missing)
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::ManifestNotFound { manifest, .. }
                    if manifest.ends_with("modle.toml"))
            })
            .lacks("compile");
    }

    #[tokio::test]
    async fn a_wired_extension_claims_the_section_an_unwired_one_refuses() {
        BootScenario::new()
            .module(acme_section_manifest())
            .expect_refusal()
            .await
            .variant::<LoadRefusal>(|e| {
                matches!(e, LoadRefusal::SectionUnclaimed { owner, section }
                    if owner == "acme-user" && section == "acme")
            })
            .lacks("read component");

        BootScenario::new()
            .extensions([Arc::new(AcmeExtension) as Arc<dyn Extension<CoreRuntime>>])
            .module(acme_section_manifest())
            .expect_refusal()
            .await
            .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound)
            // Operator wording pin.
            .names("read component")
            .lacks("no wired extension claims it");
    }

    #[tokio::test]
    #[should_panic(expected = "refusal was expected")]
    async fn expect_refusal_panics_when_boot_succeeds() {
        BootScenario::new().expect_refusal().await;
    }

    #[tokio::test]
    async fn the_scenario_store_lives_under_the_scenario_directory() {
        let scenario = BootScenario::new();
        let redb = scenario.dir().join("scenario.redb");
        let booted = scenario.boot().await.expect("an empty scenario boots");
        assert_eq!(booted.supervisor.module_count(), 0);
        assert!(redb.is_file());
    }

    #[test]
    fn entries_carry_their_component_manifest_and_operator_grants() {
        let scenario = BootScenario::new()
            .wasm("guest.wasm")
            .limits(ModuleLimits {
                poison: crate::engine_config::PoisonLimitsSection {
                    max_failures: Some(3),
                    window_secs: Some(60),
                },
                ..Default::default()
            })
            .module(TestManifest::new("a").cap("logging"))
            .module(Entry::new(TestManifest::new("b").cap("logging")).wasm("other.wasm"))
            .adapter(
                Entry::new(TestManifest::new("feed").kind("acme-feed"))
                    .http_allow(["api.acme.example"]),
            );
        // Holding _launch keeps the manifest tempdir alive for the asserts.
        let (config, _launch) = scenario.split();

        assert_eq!(config.limits.poison().max_failures, 3);
        let ids = |chains: &HashMap<Chain, ChainConfig>| {
            chains
                .keys()
                .map(|chain| chain.id())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            ids(&config.chains),
            ids(&test_chain_configs()),
            "the [chains] set defaults to the shared test configs",
        );
        assert_eq!(config.modules.len(), 2);
        assert_eq!(config.modules[0].path, Path::new("guest.wasm"));
        assert_eq!(
            config.modules[1].path,
            Path::new("other.wasm"),
            "a per-entry component overrides the scenario default",
        );
        assert_eq!(config.adapters.len(), 1);
        assert_eq!(config.adapters[0].http_allow, ["api.acme.example"]);
        for manifest in config
            .modules
            .iter()
            .map(|m| &m.manifest)
            .chain(config.adapters.iter().map(|a| &a.manifest))
        {
            assert!(
                manifest.as_deref().is_some_and(Path::is_file),
                "manifest written: {manifest:?}",
            );
        }
    }

    #[test]
    fn digest_and_defaulted_flags_reach_the_engine_config() {
        let (config, _launch) = BootScenario::new()
            .require_digest()
            .defaulted_chains()
            .split();
        assert!(config.engine.require_component_digest);
        assert!(config.defaulted);
        assert!(config.chains.is_empty());
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

    fn not_found() -> anyhow::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into()
    }

    #[test]
    fn variant_finds_the_typed_root_under_context_wraps() {
        Refusal(not_found().context("outer context"))
            .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound);
    }

    #[test]
    #[should_panic(expected = "refusal carries no")]
    fn variant_panics_on_an_absent_type() {
        Refusal(anyhow::anyhow!("boom")).variant::<std::io::Error>(|_| true);
    }

    #[test]
    #[should_panic(expected = "refusal variant mismatch")]
    fn variant_panics_on_a_failed_predicate() {
        Refusal(not_found())
            .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
    }
}
