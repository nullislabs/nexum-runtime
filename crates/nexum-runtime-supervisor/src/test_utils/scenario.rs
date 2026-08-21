//! One-expression supervisor boot through the real [`Supervisor::boot`] path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_chains::Chain;
use derive_more::From;
use tempfile::TempDir;

use super::{ManifestInput, TestManifest, in_memory_logs, test_chain_configs};
use crate::engine_config::{ChainConfig, EngineConfig, ModuleEntry, ModuleLimits, PolicySection};
use crate::error::RuntimeError;
use nexum_runtime_api::{Extension, RuntimeTypes};
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_logs::{LogPipeline, LogRecord};
use nexum_runtime_store::LocalStore;
use nexum_runtime_wasm::{Components, HostState, attach_wall_clock};

use super::test_wasmtime_engine;
use crate::supervisor::{Supervisor, WasiClockOverride, build_linker};
use nexum_primitives::digest::ContentDigest;

/// One `[[modules]]` entry.
pub struct Entry {
    id: Option<String>,
    wasm: Option<PathBuf>,
    manifest: ManifestInput,
    digest: Option<ContentDigest>,
}

impl Entry {
    /// An entry loading `manifest` on the scenario-wide component.
    pub fn new(manifest: impl Into<ManifestInput>) -> Self {
        Self {
            id: None,
            wasm: None,
            manifest: manifest.into(),
            digest: None,
        }
    }

    /// Operator-written id; unset, the entry gets `m<index>`.
    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Load this entry from `wasm` rather than the scenario-wide component.
    #[must_use]
    pub fn wasm(mut self, wasm: impl Into<PathBuf>) -> Self {
        self.wasm = Some(wasm.into());
        self
    }

    /// The operator's `[[modules]].digest` pin for this entry's artifact.
    #[must_use]
    pub fn digest(mut self, digest: ContentDigest) -> Self {
        self.digest = Some(digest);
        self
    }

    /// The `[[modules]]` row for this entry, materializing any inline
    /// manifest under `dir`. `index` names both the defaulted id and the
    /// manifest file, so a caller must number its entries uniquely.
    pub fn resolve(self, index: usize, dir: &Path, default_wasm: &Path) -> ModuleEntry {
        let mut module = ModuleEntry::new(
            self.id.unwrap_or_else(|| format!("m{index}")),
            self.wasm.unwrap_or_else(|| default_wasm.to_path_buf()),
        );
        module.manifest = self
            .manifest
            .resolve(&dir.join(format!("module-{index}.toml")));
        module.digest = self.digest;
        module
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
pub struct BootScenario<T: RuntimeTypes> {
    dir: TempDir,
    components: Components<T>,
    extensions: Vec<Arc<dyn Extension<T>>>,
    limits: ModuleLimits,
    policy: PolicySection,
    chains: HashMap<Chain, ChainConfig>,
    wasm: Option<PathBuf>,
    modules: Vec<Entry>,
    clocks: Option<WasiClockOverride>,
    require_digest: bool,
    defaulted: bool,
}

impl<T: RuntimeTypes<State = HostState<T>, Store = LocalStore>> BootScenario<T> {
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

impl<T: RuntimeTypes<State = HostState<T>>> BootScenario<T> {
    /// A scenario rooted in a fresh tempdir, holding the given backends.
    pub fn over(components: Components<T>) -> Self {
        Self::rooted(tempfile::tempdir().expect("scenario tempdir"), components)
    }

    fn rooted(dir: TempDir, components: Components<T>) -> Self {
        Self {
            dir,
            components,
            extensions: Vec::new(),
            limits: ModuleLimits::default(),
            policy: PolicySection::default(),
            chains: test_chain_configs(),
            wasm: None,
            modules: Vec::new(),
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
    #[must_use]
    pub fn wasm(mut self, wasm: impl Into<PathBuf>) -> Self {
        self.wasm = Some(wasm.into());
        self
    }

    /// Add a `[[modules]]` entry.
    #[must_use]
    pub fn module(mut self, entry: impl Into<Entry>) -> Self {
        self.modules.push(entry.into());
        self
    }

    /// Replace the whole `[limits]` section.
    #[must_use]
    pub fn limits(mut self, limits: ModuleLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replace the whole `[policy]` section.
    #[must_use]
    pub fn policy(mut self, policy: PolicySection) -> Self {
        self.policy = policy;
        self
    }

    /// Operator-permitted destinations, as `[limits.http].permit_destinations`.
    /// A test serving over loopback needs this: the address rules refuse
    /// loopback by default and a module allowlist cannot widen them.
    #[must_use]
    pub fn permit_destinations(
        mut self,
        addrs: impl IntoIterator<Item = std::net::IpAddr>,
    ) -> Self {
        self.limits.http.permit_destinations = addrs.into_iter().collect();
        self
    }

    /// Replace the `[chains]` set; defaults to [`test_chain_configs`].
    #[must_use]
    pub fn chains(mut self, chains: HashMap<Chain, ChainConfig>) -> Self {
        self.chains = chains;
        self
    }

    /// Model a run without any engine.toml: no chains, defaulted config.
    #[must_use]
    pub fn defaulted_chains(mut self) -> Self {
        self.chains = HashMap::new();
        self.defaulted = true;
        self
    }

    /// Refuse any entry whose manifest lacks a component digest pin.
    #[must_use]
    pub fn require_digest(mut self) -> Self {
        self.require_digest = true;
        self
    }

    /// Wire extensions; they reach the linker and the boot gates together.
    #[must_use]
    pub fn extensions(
        mut self,
        extensions: impl IntoIterator<Item = Arc<dyn Extension<T>>>,
    ) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Unset keeps the ambient host clocks.
    #[must_use]
    pub fn clock(mut self, clocks: WasiClockOverride) -> Self {
        self.clocks = Some(clocks);
        self
    }

    /// Write the manifests, build the engine, and boot the supervisor.
    /// The error side is what a refusal test asserts on.
    pub async fn boot(self) -> Result<Booted<T>, RuntimeError> {
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

        let mut config = EngineConfig::default();
        config.limits = self
            .limits
            .try_into()
            .expect("scenario [limits] must carry no zero");
        config.policy = self.policy;
        config.chains = self.chains;
        config.engine.state_dir = dir.clone();
        config.engine.require_component_digest = self.require_digest;
        config.defaulted = self.defaulted;
        for (i, entry) in self.modules.into_iter().enumerate() {
            config.modules.push(entry.resolve(i, &dir, &default_wasm));
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

impl<T: RuntimeTypes<State = HostState<T>, Store = LocalStore>> Default for BootScenario<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Booted supervisor; the held tempdir keeps its manifests and store alive.
pub struct Booted<T: RuntimeTypes> {
    /// The live supervisor, for dispatching and for counts.
    pub supervisor: Supervisor<T>,
    logs: LogPipeline,
    _dir: TempDir,
}

impl<T: RuntimeTypes<State = HostState<T>>> Booted<T> {
    /// The log pipeline, for reading what a module emitted.
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

/// A boot error under assertion. Wraps [`RuntimeError`] so a test can
/// reach the typed root instead of matching on a `Display` substring.
#[derive(Debug, From)]
pub struct Refusal(RuntimeError);

impl Refusal {
    /// The typed root under the context wraps, for `matches!` on a variant.
    pub fn root<E: std::error::Error + Send + Sync + 'static>(&self) -> Option<&E> {
        let top: &(dyn std::error::Error + 'static) = &self.0;
        if let Some(err) = top.downcast_ref::<E>() {
            return Some(err);
        }
        // The typed cause sits inside the `RuntimeError` value, not as a
        // chain element of its own; a nested `RuntimeError` (one a hook
        // boxed) unwraps the same way.
        let mut cause = Some(self.0.cause());
        while let Some(current) = cause {
            if let Some(err) = current.downcast_ref::<E>() {
                return Some(err);
            }
            cause = match current.downcast_ref::<RuntimeError>() {
                Some(nested) => Some(nested.cause()),
                None => current.source(),
            };
        }
        None
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
        let mut rendered = self.0.to_string();
        let mut cause = std::error::Error::source(&self.0);
        while let Some(current) = cause {
            rendered.push_str(": ");
            rendered.push_str(&current.to_string());
            cause = current.source();
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{NamespaceCaps, ParseError};
    use crate::supervisor::load::LoadRefusal;
    use crate::supervisor::prepass::BootRefusal;
    use crate::test_utils::LocalTypes;
    use crate::test_utils::{example_wasm_or_skip, module_wasm_or_skip};
    use nexum_runtime_api::HostWallClock;

    fn scenario() -> BootScenario<LocalTypes> {
        BootScenario::new()
    }

    /// Claims the `[acme]` manifest section and nothing else.
    struct AcmeExtension;

    impl Extension<LocalTypes> for AcmeExtension {
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
            _linker: &mut wasmtime::component::Linker<nexum_runtime_wasm::HostState<LocalTypes>>,
        ) -> Result<(), nexum_runtime_api::ExtensionError> {
            Ok(())
        }

        fn manifest_sections(&self) -> &'static [&'static str] {
            &["acme"]
        }
    }

    /// Records the wall clock the boot path hands the extension seam.
    struct ClockCaptureExtension(Arc<std::sync::OnceLock<Arc<dyn HostWallClock + Send + Sync>>>);

    impl Extension<LocalTypes> for ClockCaptureExtension {
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
            _linker: &mut wasmtime::component::Linker<nexum_runtime_wasm::HostState<LocalTypes>>,
        ) -> Result<(), nexum_runtime_api::ExtensionError> {
            Ok(())
        }

        fn attach_clock(&self, wall: Arc<dyn HostWallClock + Send + Sync>) {
            let _ = self.0.set(wall);
        }
    }

    /// A manifest declaring the `[acme]` section, which no builder emits.
    fn acme_section_manifest() -> String {
        "[component]\nname = \"acme-user\"\n\n[dependencies]\n\n[acme]\nventure = 1\n".to_owned()
    }

    #[tokio::test]
    async fn a_scenario_module_boots_alive_and_takes_dispatch() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let mut booted = scenario()
            .wasm(&wasm)
            .module(TestManifest::new("example").cap("logging").block_trigger(1))
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
        let mut booted = scenario()
            .wasm(&example)
            .module(TestManifest::new("example").cap("logging").block_trigger(1))
            .module(
                Entry::new(
                    TestManifest::new("clock-reader")
                        .cap("logging")
                        .block_trigger(1),
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

        use crate::test_utils::ManualClock;

        let Some(wasm) = module_wasm_or_skip("clock-reader") else {
            return;
        };
        // Far from the ambient clock; an exact match can only come from the override.
        const PINNED_SECS: u64 = 1_700_000_000;

        let clock = ManualClock::new();
        clock.set(UNIX_EPOCH + Duration::from_secs(PINNED_SECS));
        let seen = Arc::new(OnceLock::new());
        let capture: Arc<dyn Extension<LocalTypes>> = Arc::new(ClockCaptureExtension(seen.clone()));
        let mut booted = scenario()
            .wasm(&wasm)
            .module(
                TestManifest::new("clock-reader")
                    .cap("logging")
                    .block_trigger(1),
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
        scenario()
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
    async fn a_component_without_any_manifest_refuses_on_discovery() {
        let scenario = scenario();
        let orphan = scenario.dir().join("orphan.wasm");
        scenario
            .module(Entry::new(ManifestInput::Beside).wasm(orphan))
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
        let scenario = scenario();
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
        scenario()
            .module(acme_section_manifest())
            .expect_refusal()
            .await
            .variant::<LoadRefusal>(|e| {
                matches!(e, LoadRefusal::SectionUnclaimed { owner, section }
                    if owner == "acme-user" && section == "acme")
            })
            .lacks("read component");

        scenario()
            .extensions([Arc::new(AcmeExtension) as Arc<dyn Extension<LocalTypes>>])
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
        scenario().expect_refusal().await;
    }

    #[tokio::test]
    async fn the_scenario_store_lives_under_the_scenario_directory() {
        let scenario = scenario();
        let redb = scenario.dir().join("scenario.redb");
        let booted = scenario.boot().await.expect("an empty scenario boots");
        assert_eq!(booted.supervisor.module_count(), 0);
        assert!(redb.is_file());
    }

    #[test]
    fn entries_carry_their_component_manifest_and_operator_limits() {
        let scenario = scenario()
            .wasm("guest.wasm")
            .limits(crate::test_utils::limits_with(|limits| {
                limits.poison = crate::engine_config::PoisonLimitsSection {
                    max_failures: Some(3),
                    window_secs: Some(60),
                }
            }))
            .module(TestManifest::new("a").cap("logging"))
            .module(Entry::new(TestManifest::new("b").cap("logging")).wasm("other.wasm"));
        // Holding _launch keeps the manifest tempdir alive for the asserts.
        let (config, _launch) = scenario.split();

        assert_eq!(config.limits.poison.max_failures.get(), 3);
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
        for manifest in config.modules.iter().map(|m| &m.manifest) {
            assert!(
                manifest.as_deref().is_some_and(Path::is_file),
                "manifest written: {manifest:?}",
            );
        }
    }

    #[test]
    fn digest_and_defaulted_flags_reach_the_engine_config() {
        let (config, _launch) = scenario().require_digest().defaulted_chains().split();
        assert!(config.engine.require_component_digest);
        assert!(config.defaulted);
        assert!(config.chains.is_empty());
    }

    /// A refusal over the engine arm.
    fn wrapped(err: anyhow::Error) -> Refusal {
        Refusal(crate::error::EngineRefusal::new(err).into())
    }

    #[test]
    fn names_and_lacks_read_the_whole_context_chain() {
        let refusal = wrapped(anyhow::anyhow!("root cause").context("outer context"));
        refusal
            .names("outer context")
            .names("root cause")
            .lacks("absent needle");
    }

    #[test]
    #[should_panic(expected = "refusal does not name")]
    fn names_panics_on_an_absent_needle() {
        wrapped(anyhow::anyhow!("boom")).names("quiet");
    }

    #[test]
    #[should_panic(expected = "refusal names")]
    fn lacks_panics_on_a_present_needle() {
        wrapped(anyhow::anyhow!("boom")).lacks("boom");
    }

    fn not_found() -> anyhow::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into()
    }

    #[test]
    fn variant_finds_the_typed_root_under_context_wraps() {
        wrapped(not_found().context("outer context"))
            .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound);
    }

    #[test]
    fn variant_finds_a_runtime_error_arm_at_the_top() {
        Refusal(RuntimeError::from(
            crate::error::LaunchRefusal::NothingToRun,
        ))
        .variant::<RuntimeError>(|e| {
            matches!(
                e,
                RuntimeError::Launch(crate::error::LaunchRefusal::NothingToRun)
            )
        });
    }

    #[test]
    #[should_panic(expected = "refusal carries no")]
    fn variant_panics_on_an_absent_type() {
        wrapped(anyhow::anyhow!("boom")).variant::<std::io::Error>(|_| true);
    }

    #[test]
    #[should_panic(expected = "refusal variant mismatch")]
    fn variant_panics_on_a_failed_predicate() {
        wrapped(not_found())
            .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
    }
}
