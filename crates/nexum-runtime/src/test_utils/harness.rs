//! In-process test harness: launch one module over the mock assembly and
//! drive it from a test.
//!
//! [`TestRuntime`] wraps the public builder path over [`MockTypes`] with a
//! manually-driven [`ManualClock`]; the chain leg is the real
//! [`ProviderPool`](crate::host::provider_pool::ProviderPool) over a routed
//! [`FakeNode`] transport. Program the mocks and read effects through
//! [`chain`](TestRuntime::chain), [`clock`](TestRuntime::clock),
//! [`store`](TestRuntime::store) and [`logs`](TestRuntime::logs). Events
//! dispatch on the spawned event-loop task, so
//! [`wait_for_log`](TestRuntime::wait_for_log) polls for an observable
//! effect. [`TestRuntimeBuilder::boot_supervisor`] instead hands back the
//! booted [`Supervisor`](crate::supervisor::Supervisor) over the same mocks
//! for direct dispatch, with no event loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy_chains::Chain;
use alloy_rpc_types_eth::{Header, Log};

use super::clock::ManualClock;
use super::manifest::ManifestInput;
use super::rpc::FakeNode;
use super::scenario::{BootScenario, Booted, Entry};
use super::{HARNESS_POLL_INTERVAL, MockStateStore, MockTypes, Prebuilt};
use crate::builder::{RuntimeBuilder, RuntimeHandle};
use crate::engine_config::{EngineConfig, ModuleLimits};
use crate::error::{BoxError, RuntimeError};
use crate::host::component::{Components, ComponentsBuilder};
use crate::host::extension::Extension;
use crate::host::logs::{LogPipeline, LogRecord};

/// Builder for a [`TestRuntime`]; the launched handle shares the same mock
/// backends. A manifest is mandatory.
pub struct TestRuntimeBuilder {
    wasm: PathBuf,
    manifest: ManifestInput,
    extensions: Vec<Arc<dyn Extension<MockTypes>>>,
    limits: ModuleLimits,
    chain: FakeNode,
    chains: Vec<Chain>,
    store: MockStateStore,
    clock: ManualClock,
}

impl TestRuntime {
    /// Start a harness for the module at `wasm`.
    pub fn builder(wasm: impl Into<PathBuf>) -> TestRuntimeBuilder {
        TestRuntimeBuilder {
            wasm: wasm.into(),
            manifest: ManifestInput::Beside,
            extensions: Vec::new(),
            limits: ModuleLimits::default(),
            chain: FakeNode::new(),
            chains: vec![Chain::from_id(1)],
            store: MockStateStore::new(),
            clock: ManualClock::new(),
        }
    }
}

impl TestRuntimeBuilder {
    /// Load the manifest from an existing file.
    #[must_use]
    pub fn manifest_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.manifest = ManifestInput::Path(path.into());
        self
    }

    /// Write `toml` to a temp file at launch and load the module from it.
    #[must_use]
    pub fn manifest_inline(mut self, toml: impl Into<String>) -> Self {
        self.manifest = ManifestInput::Toml(toml.into());
        self
    }

    /// Register an extension.
    #[must_use]
    pub fn extension(mut self, extension: Arc<dyn Extension<MockTypes>>) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Register several extensions at once.
    #[must_use]
    pub fn extensions(
        mut self,
        extensions: impl IntoIterator<Item = Arc<dyn Extension<MockTypes>>>,
    ) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Replace the `[limits]` the launch resolves; defaults to the
    /// production defaults.
    #[must_use]
    pub fn limits(mut self, limits: ModuleLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The fake chain node; the launched handle shares this instance.
    pub fn chain(&self) -> &FakeNode {
        &self.chain
    }

    /// The mock state store; the launched handle shares this instance.
    pub fn store(&self) -> &MockStateStore {
        &self.store
    }

    /// The manual clock installed as the per-store WASI clock override.
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// Open the module and start the runtime through the public builder path.
    pub async fn launch(self) -> Result<TestRuntime, RuntimeError> {
        // A temp directory roots any inline manifest and stands in as the
        // (unused, in-memory backends) state directory.
        let tmp = tempfile::tempdir().expect("harness tempdir");

        let manifest = self.manifest.resolve(&tmp.path().join("component.toml"));

        let mut config = EngineConfig::default();
        config.engine.state_dir = tmp.path().to_path_buf();
        config.limits = self.limits.try_into()?;
        // The chain gate applies even over mock backends.
        config.chains = super::test_chain_configs();

        let pool = self.chain.pool(&self.chains, HARNESS_POLL_INTERVAL);
        let handle = RuntimeBuilder::new(&config)
            .with_types::<MockTypes>()
            .with_extensions(self.extensions)
            .with_module_source(Some(self.wasm), manifest)
            .with_wasi_clocks(self.clock.as_override())
            .with_components(ComponentsBuilder::new(
                Prebuilt(pool),
                Prebuilt(self.store.clone()),
            ))
            .launch()
            .await?;

        Ok(TestRuntime {
            handle,
            chain: self.chain,
            store: self.store,
            clock: self.clock,
            _tmp: tmp,
        })
    }

    /// Boots through the real admission path over the builder's mocks and
    /// returns the supervisor, without the event loop
    /// [`launch`](Self::launch) spawns.
    ///
    /// Consumes the builder, so clone the [`chain`](Self::chain),
    /// [`store`](Self::store) and [`clock`](Self::clock) handles first if
    /// you still need to drive the mocks.
    pub async fn boot_supervisor(self) -> Result<Booted<MockTypes>, RuntimeError> {
        let pool = self.chain.pool(&self.chains, HARNESS_POLL_INTERVAL);
        BootScenario::over(Components {
            chain: pool,
            store: self.store,
            logs: super::in_memory_logs(),
        })
        .wasm(self.wasm)
        .module(Entry::new(self.manifest))
        .extensions(self.extensions)
        .limits(self.limits)
        .clock(self.clock.as_override())
        .boot()
        .await
    }
}

/// A launched in-process runtime over the mock assembly; dropping it fires
/// the shutdown signal.
pub struct TestRuntime {
    handle: RuntimeHandle,
    chain: FakeNode,
    store: MockStateStore,
    clock: ManualClock,
    // Holds any inline manifest for the lifetime of the harness; dropped
    // when the `TestRuntime` is dropped (or consumed by `wait`).
    _tmp: tempfile::TempDir,
}

impl TestRuntime {
    /// The fake node the running engine is wired to, for scripting
    /// responses while it runs.
    pub fn chain(&self) -> &FakeNode {
        &self.chain
    }

    /// The mock state store, for asserting on what a module wrote.
    pub fn store(&self) -> &MockStateStore {
        &self.store
    }

    /// The manual clock driving guest-visible time.
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// The shared log pipeline.
    pub fn logs(&self) -> &LogPipeline {
        self.handle.logs()
    }

    /// Deliver a block header to the module's open block stream.
    pub fn push_block(&self, header: Header) {
        self.chain.push_block(header);
    }

    /// Deliver a log to the module's open chain-log stream.
    pub fn push_chain_log(&self, log: Log) {
        self.chain.push_chain_log(log);
    }

    /// Await a `module` log record whose message contains `needle`.
    /// Notification-driven, so it resolves as soon as the dispatched event's
    /// record lands; the 5s bound is a failure backstop.
    pub async fn wait_for_log(&self, module: &str, needle: &str) -> Result<LogRecord, BoxError> {
        let logs = self.logs();
        let appended = logs.appended();
        let matched = async {
            loop {
                // Arm the waiter before reading so an append landing between
                // the read and the await wakes us rather than being lost.
                let notified = appended.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(record) = logs.list_runs(module).into_iter().find_map(|meta| {
                    logs.read(&meta.run, 0)
                        .records
                        .into_iter()
                        .find(|record| record.message.contains(needle))
                }) {
                    return record;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), matched)
            .await
            .map_err(|_| {
                BoxError::from(format!(
                    "no {module} log record matched {needle:?} within 5s"
                ))
            })
    }

    /// Signal the event loop to stop; the in-flight dispatch finishes first.
    pub fn shutdown(&mut self) {
        self.handle.shutdown();
    }

    /// Await the event loop's completion after a [`shutdown`](Self::shutdown).
    pub async fn wait(self) -> Result<(), RuntimeError> {
        self.handle.wait().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::host::extension::Extension;
    use crate::manifest::NamespaceCaps;
    use crate::test_utils::{TestManifest, example_wasm_or_skip, manifest, module_wasm_or_skip};

    fn example_block_manifest() -> String {
        manifest("example")
            .cap("logging")
            .block_trigger(1)
            .to_toml()
    }

    /// A block manifest plus a `[component].digest` pin of the wasm's bytes.
    fn pinned_block_manifest(name: &str, chain_id: u64, wasm: &std::path::Path) -> String {
        let digest = crate::digest::ContentDigest::of_bytes(
            &std::fs::read(wasm).expect("read module wasm for pinning"),
        );
        manifest(name)
            .cap("logging")
            .component_digest(digest.to_string())
            .block_trigger(chain_id)
            .to_toml()
    }

    fn price_alert_manifest() -> String {
        manifest("price-alert")
            .require(["logging", "chain"])
            .block_trigger(1)
            .config(
                "oracle_address",
                "0x694AA1769357215DE4FAC081bf1f309aDC325306",
            )
            .config("decimals", "8")
            .config("threshold", "2500.00")
            .config("direction", "above")
            .to_toml()
    }

    /// A header carrying just the block number.
    fn header_numbered(number: u64) -> Header {
        let mut header: Header = Header::default();
        header.inner.number = number;
        header
    }

    /// End-to-end: launch the example module from an inline manifest, inject
    /// a block header, and read the module's log line back.
    #[tokio::test]
    async fn harness_launches_dispatches_and_reads_logs() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .launch()
            .await
            .expect("launch example over the harness");

        rt.push_block(header_numbered(19_000_000));
        let record = rt
            .wait_for_log("example", "block 19000000")
            .await
            .expect("the on_trigger log line lands after dispatch");
        assert_eq!(
            record.channel,
            crate::host::logs::LogChannel::HostInterface,
            "the example module logs through the host interface",
        );

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn harness_launches_with_a_pinned_component_digest() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let manifest = pinned_block_manifest("example", 1, &wasm);
        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(manifest)
            .launch()
            .await
            .expect("launch the pinned example over the harness");

        rt.push_block(header_numbered(19_000_001));
        rt.wait_for_log("example", "block 19000001")
            .await
            .expect("the pinned module dispatches after strict verification");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// End-to-end on the event leg: launch with an `event`
    /// trigger, inject a log, and read the module's log line back.
    #[tokio::test]
    async fn harness_dispatches_events() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(
                TestManifest::new("example")
                    .cap("logging")
                    .event_trigger(1)
                    .to_toml(),
            )
            .launch()
            .await
            .expect("launch example on the event leg");

        rt.push_chain_log(Log::default());
        rt.wait_for_log("example", "event with 0 topics on chain 1")
            .await
            .expect("the event line lands after dispatch");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// An extension threads through the harness: its linker hook runs at
    /// boot and the module still dispatches.
    #[tokio::test]
    async fn harness_threads_an_extension() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        struct CountingExtension(Arc<AtomicUsize>);

        impl Extension<MockTypes> for CountingExtension {
            fn namespace(&self) -> &'static str {
                "test"
            }
            fn capabilities(&self) -> NamespaceCaps {
                NamespaceCaps {
                    prefix: "test:ext/",
                    ifaces: &[],
                }
            }
            fn link(
                &self,
                _linker: &mut wasmtime::component::Linker<crate::host::state::HostState<MockTypes>>,
            ) -> Result<(), crate::host::extension::ExtensionError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let extension = Arc::new(CountingExtension(calls.clone()));

        let mut rt = TestRuntime::builder(wasm)
            .extension(extension)
            .manifest_inline(example_block_manifest())
            .launch()
            .await
            .expect("launch with a trivial extension");

        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the extension linker hook ran at boot",
        );

        rt.push_block(header_numbered(21_000_000));
        rt.wait_for_log("example", "block 21000000")
            .await
            .expect("the module dispatched with the extension linked");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// [`TestRuntimeBuilder::limits`] reaches the launch: a one-byte log ring
    /// keeps only the newest record, evicting the init line.
    #[tokio::test]
    async fn harness_threads_module_limits() {
        use crate::engine_config::LogLimitsSection;

        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .limits(ModuleLimits {
                logs: LogLimitsSection {
                    bytes_per_run: Some(1),
                    runs_retained: None,
                },
                ..Default::default()
            })
            .launch()
            .await
            .expect("launch example with tight log limits");

        rt.push_block(header_numbered(19_000_000));
        rt.wait_for_log("example", "block 19000000")
            .await
            .expect("the on_trigger log line lands after dispatch");

        let runs = rt.logs().list_runs("example");
        assert_eq!(runs.len(), 1, "one run recorded");
        let page = rt.logs().read(&runs[0].run, 0);
        assert_eq!(
            page.records.len(),
            1,
            "the one-byte ring keeps only the newest record",
        );
        assert!(page.records[0].message.contains("block 19000000"));

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// End to end on the chain-request leg: program the mock `eth_call`,
    /// launch price-alert, inject a block, and read its alert line back; the
    /// programmed answer is above threshold, so the module logs the alert.
    #[tokio::test]
    async fn harness_serves_chain_requests_to_the_module() {
        use crate::host::component::ChainMethod;

        let Some(wasm) = module_wasm_or_skip("price-alert") else {
            return;
        };

        /// One 32-byte ABI word as zero-padded hex.
        fn word(v: u128) -> String {
            format!("{v:064x}")
        }
        // latestRoundData() -> (roundId, answer, startedAt, updatedAt,
        // answeredInRound), answer = 3000 * 10^8, above the 2500.00
        // threshold below.
        let result = format!(
            "\"0x{}{}{}{}{}\"",
            word(1),
            word(300_000_000_000),
            word(0),
            word(0),
            word(1),
        );

        let builder = TestRuntime::builder(wasm).manifest_inline(price_alert_manifest());
        builder.chain().on_method(ChainMethod::EthCall, result);

        let mut rt = builder
            .launch()
            .await
            .expect("launch price-alert over the harness");

        rt.push_block(header_numbered(19_000_000));
        rt.wait_for_log("price-alert", "THRESHOLD CROSSED")
            .await
            .expect("the alert line lands after the oracle read");

        let requests = rt.chain().recorded_requests();
        assert!(
            requests.iter().any(|r| {
                r.method == "eth_call"
                    && r.params
                        .to_string()
                        .contains("0x694aa1769357215de4fac081bf1f309adc325306")
            }),
            "the module's eth_call reached the fake node, got: {requests:?}",
        );

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// Both block and event triggers dispatch in one session: the `biased`
    /// select in `run()` delivers both kinds without starvation.
    #[tokio::test]
    async fn harness_delivers_block_and_event_triggers_without_starvation() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(
                TestManifest::new("example")
                    .cap("logging")
                    .block_trigger(1)
                    .event_trigger(1)
                    .to_toml(),
            )
            .launch()
            .await
            .expect("launch example declaring both blocks and events");

        // Both events are queued before either is awaited, so the biased
        // select genuinely arbitrates between two ready streams: a
        // sequential push→wait→push→wait would never create contention.
        // The log shares height 42 so neither poller starts past the block.
        rt.push_block(header_numbered(42));
        rt.push_chain_log(Log {
            block_number: Some(42),
            ..Default::default()
        });

        rt.wait_for_log("example", "block 42 on chain")
            .await
            .expect("block event dispatched");
        rt.wait_for_log("example", "event with 0 topics on chain 1")
            .await
            .expect("event dispatched, neither trigger kind starved the other");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// Blocks pushed in order arrive in the same order; the stream, select,
    /// and dispatch path preserve delivery order, asserted on the module's
    /// own log records.
    #[tokio::test]
    async fn harness_delivers_blocks_in_push_order() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .launch()
            .await
            .expect("launch example over the harness");

        // Await each delivery before pushing the next height.
        rt.push_block(header_numbered(7));
        rt.wait_for_log("example", "block 7 on chain")
            .await
            .expect("first block dispatched");
        rt.push_block(header_numbered(8));
        rt.wait_for_log("example", "block 8 on chain")
            .await
            .expect("second block dispatched");
        rt.push_block(header_numbered(9));
        rt.wait_for_log("example", "block 9 on chain")
            .await
            .expect("final block dispatched");

        // Recover the per-block log lines in record order and assert the
        // sequence matches the push order exactly.
        let logs = rt.logs();
        let numbers: Vec<u64> = logs
            .list_runs("example")
            .into_iter()
            .flat_map(|meta| logs.read(&meta.run, 0).records)
            .filter_map(|record| {
                let rest = record.message.strip_prefix("block ")?;
                rest.split(' ').next()?.parse().ok()
            })
            .collect();
        assert_eq!(
            numbers,
            vec![7, 8, 9],
            "blocks must be dispatched in push order",
        );

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// Shutdown never destroys completed work: a picked-up block finishes its
    /// wasmtime call and its log record survives `wait()`. Proven by
    /// re-reading the record after full teardown.
    #[tokio::test]
    async fn harness_shutdown_preserves_completed_dispatch() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .launch()
            .await
            .expect("launch example over the harness");

        rt.push_block(header_numbered(1));
        rt.wait_for_log("example", "block 1 on chain")
            .await
            .expect("dispatch completed before shutdown");

        let logs = rt.logs().clone();
        rt.shutdown();
        rt.wait().await.expect("no panic or corruption on shutdown");

        let survived = logs.list_runs("example").into_iter().any(|meta| {
            logs.read(&meta.run, 0)
                .records
                .iter()
                .any(|r| r.message.contains("block 1 on chain"))
        });
        assert!(
            survived,
            "the completed dispatch's log record must survive engine teardown",
        );
    }

    /// `[limits.chain].response_body_max_bytes` is enforced on the real
    /// `chain::request` path: an over-cap response is rejected before the
    /// guest copy, and the module observes the typed `invalid-input` fault.
    #[tokio::test]
    async fn harness_enforces_chain_response_cap_on_the_request_path() {
        use crate::engine_config::ChainLimitsSection;
        use crate::host::component::ChainMethod;

        let Some(wasm) = module_wasm_or_skip("price-alert") else {
            return;
        };

        // A syntactically valid oracle answer, ~330 bytes - far over the
        // 16-byte cap below, so the module must never see it.
        fn word(v: u128) -> String {
            format!("{v:064x}")
        }
        let result = format!(
            "\"0x{}{}{}{}{}\"",
            word(1),
            word(300_000_000_000),
            word(0),
            word(0),
            word(1),
        );

        let builder = TestRuntime::builder(wasm)
            .manifest_inline(price_alert_manifest())
            .limits(ModuleLimits {
                chain: ChainLimitsSection {
                    response_body_max_bytes: Some(16),
                },
                ..Default::default()
            });
        builder.chain().on_method(ChainMethod::EthCall, result);

        let mut rt = builder
            .launch()
            .await
            .expect("launch price-alert with a 16-byte chain response cap");

        rt.push_block(header_numbered(19_000_000));
        let record = rt
            .wait_for_log("price-alert", "exceeds the configured cap")
            .await
            .expect("the module logs the guest-visible cap fault");
        assert!(
            record.message.contains("eth_call failed"),
            "the cap surfaces as a failed eth_call, got: {}",
            record.message,
        );

        // The module never saw the oracle answer, so it must not alert.
        let runs = rt.logs().list_runs("price-alert");
        let alerted = runs.into_iter().any(|meta| {
            rt.logs()
                .read(&meta.run, 0)
                .records
                .iter()
                .any(|r| r.message.contains("THRESHOLD CROSSED"))
        });
        assert!(!alerted, "an over-cap response must never reach classify");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn harness_resumes_dispatch_after_a_transport_error() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .launch()
            .await
            .expect("launch example over the harness");

        rt.push_block(header_numbered(41));
        rt.wait_for_log("example", "block 41 on chain")
            .await
            .expect("the pre-error block dispatches");

        rt.chain().fail_head_fetches(1);
        rt.push_block(header_numbered(42));
        rt.wait_for_log("example", "block 42 on chain")
            .await
            .expect("dispatch resumes once the head poll recovers");

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// The guest observes the `WasiClockOverride`: pin the harness clock,
    /// dispatch a block, and check the clock-reader fixture logs the pinned
    /// wall time, not the ambient host clock.
    #[tokio::test]
    async fn harness_guest_observes_the_clock_override() {
        use std::time::{Duration, UNIX_EPOCH};

        let Some(wasm) = module_wasm_or_skip("clock-reader") else {
            return;
        };

        // A round instant far from the ambient clock: a stale ambient read
        // would land in the 1.7-billion-plus range of the present, so an
        // exact match on this value can only come from the override.
        const PINNED_SECS: u64 = 1_700_000_000;

        let builder = TestRuntime::builder(wasm).manifest_inline(
            TestManifest::new("clock-reader")
                .cap("logging")
                .block_trigger(1)
                .to_toml(),
        );
        builder
            .clock()
            .set(UNIX_EPOCH + Duration::from_secs(PINNED_SECS));

        let mut rt = builder
            .launch()
            .await
            .expect("launch clock-reader over the harness");

        rt.push_block(header_numbered(19_000_000));
        let record = rt
            .wait_for_log("clock-reader", &format!("clock wall {PINNED_SECS}"))
            .await
            .expect("the guest logs its wall-clock reading after dispatch");

        // The line is a host-interface log carrying exactly the pinned
        // seconds, parsed back to guard against a substring false positive.
        assert_eq!(
            record.channel,
            crate::host::logs::LogChannel::HostInterface,
            "the fixture logs through the host interface",
        );
        let logged: u64 = record
            .message
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("the log line ends in the wall-clock seconds");
        assert_eq!(
            logged, PINNED_SECS,
            "the guest read the overridden wall clock, not the ambient host clock",
        );

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// The guest sees no host environment, no process arguments, and no
    /// stdin: the env-reader fixture counts all three through std, which
    /// routes to the ambient `wasi:cli/environment` and `wasi:cli/stdin`
    /// interfaces. The host process carries environment and arguments, so
    /// a store that started inheriting either would report a non-zero
    /// count here, and an inherited stdin would report its bytes or block.
    #[tokio::test]
    async fn harness_guest_observes_no_environment_arguments_or_stdin() {
        let Some(wasm) = module_wasm_or_skip("env-reader") else {
            return;
        };

        // The precondition that makes a zero reading meaningful: an
        // inherited context could not answer with nothing.
        assert!(
            std::env::vars_os().next().is_some(),
            "the test process must carry environment variables",
        );
        assert!(
            std::env::args_os().next().is_some(),
            "the test process must carry process arguments",
        );

        let mut rt = TestRuntime::builder(wasm)
            .manifest_inline(
                TestManifest::new("env-reader")
                    .cap("logging")
                    .block_trigger(1)
                    .to_toml(),
            )
            .launch()
            .await
            .expect("launch env-reader over the harness");

        rt.push_block(header_numbered(19_000_000));
        let record = rt
            .wait_for_log("env-reader", "env vars ")
            .await
            .expect("the guest logs its environment observation after dispatch");

        // Exact match, not substring: on a leak the fixture also logs each
        // key and argument, so the counts line names the failure precisely.
        assert_eq!(
            record.message, "env vars 0 args 0 stdin 0",
            "the guest observed the host environment, arguments, or stdin",
        );

        rt.shutdown();
        rt.wait().await.expect("clean shutdown");
    }

    /// [`TestRuntimeBuilder::boot_supervisor`] reaches the alive count and
    /// dispatch without a boot entry or an event loop.
    #[tokio::test]
    async fn boot_supervisor_exposes_the_booted_supervisor() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };

        let mut booted = TestRuntime::builder(wasm)
            .manifest_inline(example_block_manifest())
            .boot_supervisor()
            .await
            .expect("boot the example module to a supervisor");

        assert_eq!(booted.supervisor.module_count(), 1);
        assert_eq!(booted.supervisor.alive_count(), 1);

        assert_eq!(
            booted.dispatch_block_on(1).await,
            1,
            "the direct dispatch reaches the one booted module",
        );
        assert!(
            booted
                .records("example")
                .iter()
                .any(|record| record.message.contains("block 19000000")),
            "the dispatched block's log line lands without an event loop",
        );
    }

    /// A cloned [`FakeNode`] still serves and records after the boot
    /// consumes the builder.
    #[tokio::test]
    async fn boot_supervisor_serves_chain_requests_from_the_builder_mocks() {
        use crate::host::component::ChainMethod;

        let Some(wasm) = module_wasm_or_skip("price-alert") else {
            return;
        };

        /// One 32-byte ABI word as zero-padded hex.
        fn word(v: u128) -> String {
            format!("{v:064x}")
        }
        // latestRoundData() with answer = 3000 * 10^8, above the manifest's
        // 2500.00 threshold, so the module logs the alert.
        let result = format!(
            "\"0x{}{}{}{}{}\"",
            word(1),
            word(300_000_000_000),
            word(0),
            word(0),
            word(1),
        );

        let builder = TestRuntime::builder(wasm).manifest_inline(price_alert_manifest());
        builder.chain().on_method(ChainMethod::EthCall, result);
        // The boot consumes the builder; the cloned handle shares the node.
        let chain = builder.chain().clone();

        let mut booted = builder
            .boot_supervisor()
            .await
            .expect("boot price-alert to a supervisor");

        assert_eq!(booted.dispatch_block_on(1).await, 1);
        assert!(
            booted
                .records("price-alert")
                .iter()
                .any(|record| record.message.contains("THRESHOLD CROSSED")),
            "the programmed oracle answer reached the module",
        );
        assert!(
            chain
                .recorded_requests()
                .iter()
                .any(|request| request.method == "eth_call"),
            "the module's eth_call reached the fake node through the retained handle",
        );
    }
}
