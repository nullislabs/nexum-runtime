//! Type-state runtime builder and the imperative launcher it drives.
//!
//! [`RuntimeBuilder`] accumulates the assembly (config, lattice, extensions,
//! component builders, add-ons) through a type-state chain;
//! [`ReadyBuilder::launch`] opens the backends and hands off to
//! [`AssembledRuntime::launch`], which installs add-ons, builds the engine and
//! linker, boots the supervisor, opens the sources, spawns the event
//! loop, and returns a [`RuntimeHandle`]. [`RuntimeBuilder::runtime`] binds a
//! [`Runtime`] preset for the common case.

use std::future::IntoFuture;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nexum_tasks::{DrainOutcome, TaskHandle, TaskManager, TaskSet};
use tracing::{error, info, warn};

use crate::addons::{AddOnHandle, AddOns, AddOnsContext};
use crate::engine_config::{EngineConfig, ModuleEntry, PolicySection};
use crate::error::{LaunchRefusal, RuntimeError};
use nexum_runtime_api::{BuilderContext, ComponentBuilder, Extension, RuntimeTypes, SourceContext};
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_logs::LogPipeline;
use nexum_runtime_supervisor::event_loop;
use nexum_runtime_supervisor::event_loop::RunEnd;
use nexum_runtime_supervisor::supervisor::count_boot_refusal;
use nexum_runtime_wasm::{Components, ComponentsBuilder, HostState, attach_wall_clock};

use crate::preset::Runtime;
pub use crate::supervisor::WasiClockOverride;
use crate::supervisor::{self, Supervisor, Viability};

/// Ambient inputs the launcher reads.
pub struct LaunchContext<'a> {
    /// Owns task spawning and graceful shutdown for the run.
    pub tasks: TaskManager,
    /// The loaded engine config.
    pub config: &'a EngineConfig,
}

/// A running runtime. [`shutdown`](Self::shutdown) or dropping fires shutdown;
/// [`wait`](Self::wait) blocks on the bounded drain.
pub struct RuntimeHandle {
    event_loop: TaskHandle<RunEnd>,
    tasks: TaskManager,
    logs: LogPipeline,
    // `[limits.shutdown] drain_secs`.
    drain_timeout: Duration,
    // Held for the length of the run; dropped once the event loop has joined.
    _add_ons: Vec<AddOnHandle>,
}

impl RuntimeHandle {
    /// Signal the event loop to stop. The in-flight dispatch finishes first.
    pub fn shutdown(&mut self) {
        self.tasks.shutdown_signal().fire();
    }

    /// The shared log pipeline: the read side for module runs and log pages.
    /// Clone it to keep reading after [`wait`](Self::wait) consumes the handle.
    pub fn logs(&self) -> &LogPipeline {
        &self.logs
    }

    /// Block until the loop stops (on its own, on shutdown, or on a critical
    /// task ending), bounding the final flush by `[limits.shutdown]
    /// drain_secs`; a drain past that bound forces exit 1.
    pub async fn wait(self) -> Result<(), RuntimeError> {
        let RuntimeHandle {
            event_loop,
            mut tasks,
            drain_timeout,
            _add_ons,
            ..
        } = self;
        let mut signal = tasks.subscribe();
        let join = event_loop.join();
        tokio::pin!(join);
        tokio::select! {
            biased;
            joined = &mut join => return finish_wait(joined),
            name = tasks.on_critical_failure() => {
                warn!(task = %name, "critical task ended, draining");
            }
            () = signal.recv() => {}
        }
        // Signalled: block on the bounded drain. The event-loop task holds
        // the flush guard until it returns, not the abort-only reconnect
        // pumps.
        match tasks.graceful_shutdown_with_timeout(drain_timeout).await {
            DrainOutcome::Drained => finish_wait(join.await),
            DrainOutcome::TimedOut { outstanding } => {
                error!(
                    outstanding,
                    timeout = ?drain_timeout,
                    "shutdown drain exceeded deadline, forcing exit"
                );
                // Exit 1 is a decision: the fan-out halts between guest
                // calls and the default drain outlasts the one in-flight
                // deadline-bounded call, so a timeout is a wedged task, and
                // `Restart=on-failure` should restart it.
                std::process::exit(1);
            }
        }
    }
}

/// Map an event-loop join outcome to the [`wait`](RuntimeHandle::wait) result.
/// [`RunEnd::NothingLive`] is a deliberate quiet stop and must stay `Ok`;
/// an unaccounted stream end is a dead pump and must not.
fn finish_wait(joined: Option<RunEnd>) -> Result<(), RuntimeError> {
    match joined {
        Some(RunEnd::SourceTerminal(term)) => Err(refuse_launch(LaunchRefusal::SourceTerminal {
            chain_id: term.chain_id,
            reason: term.reason,
        })),
        Some(RunEnd::Shutdown | RunEnd::NothingLive) => Ok(()),
        Some(RunEnd::StreamEnded) | None => Err(refuse_launch(LaunchRefusal::EventLoopGone)),
    }
}

/// Counts the refusal under its `error_kind`. The wait-time refusals
/// ([`LaunchRefusal::EventLoopGone`], [`LaunchRefusal::SourceTerminal`])
/// have no label, so they count nothing.
fn refuse_launch(refusal: LaunchRefusal) -> RuntimeError {
    let refusal = RuntimeError::from(refusal);
    count_boot_refusal(&refusal);
    refusal
}

/// A fully-assembled runtime: concrete backends, extensions, add-ons, and the
/// optional module-source override. [`launch`](Self::launch) runs it.
pub struct AssembledRuntime<T: RuntimeTypes> {
    /// Shared backends threaded into every module store.
    pub components: Components<T>,
    /// Extensions: namespaces, capabilities, linker hooks, and event
    /// sources.
    pub extensions: Vec<Arc<dyn Extension<T>>>,
    /// Cross-cutting facilities installed before the engine boots.
    pub add_ons: AddOns,
    /// Single-module source override; `None` runs `[[modules]]`.
    pub wasm: Option<PathBuf>,
    /// Manifest paired with `wasm`.
    pub manifest: Option<PathBuf>,
    /// Per-store WASI clock override; `None` leaves the ambient host clocks.
    pub clocks: Option<WasiClockOverride>,
}

impl<T: RuntimeTypes<State = HostState<T>>> AssembledRuntime<T> {
    /// Run the imperative launch sequence and return the running handle.
    pub async fn launch(self, ctx: LaunchContext<'_>) -> Result<RuntimeHandle, RuntimeError> {
        let AssembledRuntime {
            components,
            extensions,
            add_ons,
            wasm,
            manifest,
            clocks,
        } = self;
        let LaunchContext {
            tasks,
            config: engine_cfg,
        } = ctx;

        // Install cross-cutting add-ons before the engine boots so any metric
        // recorder is live for the whole run. The handles move into the
        // returned handle and drop once the event loop joins.
        let addons_ctx = AddOnsContext {
            metrics: &engine_cfg.engine.metrics,
        };
        let add_on_handles = add_ons
            .iter()
            .map(|add_on| add_on.install(&addons_ctx))
            .collect::<Result<Vec<_>, _>>()
            .map_err(RuntimeError::AddOn)?;

        // wasmtime engine + linker - one of each, shared across modules.
        let engine = nexum_runtime_supervisor::supervisor::engine()?;

        // Extensions receive the effective wall clock before linking, so
        // their host-side time and guest WASI time share one source.
        attach_wall_clock(&extensions, clocks.as_ref());
        let linker = supervisor::build_linker::<T>(&engine, &extensions)?;

        // Boot supervisor - a module-source override wins over
        // `engine.toml.[[modules]]`.
        let wasm_override = wasm.is_some();
        let supervisor = if let Some(wasm) = wasm {
            if !engine_cfg.modules.is_empty() {
                warn!(
                    "ignoring engine.toml [[modules]] because a module source override was given"
                );
            }
            if !engine_cfg.policy.component.is_empty() {
                warn!(
                    "ignoring engine.toml [policy.component] rows: the override is not a \
                     configured component, so it gets the [policy] defaults"
                );
            }
            // The override is not any configured component, and its file
            // stem is not an operator-written id, so no [policy.component]
            // row may bind to it (ADR-0018); the stem is display-only.
            let policy = PolicySection {
                component: Default::default(),
                ..engine_cfg.policy.clone()
            };
            let env = supervisor::BootEnv {
                policy: &policy,
                // No [[modules]] entry describes the override, so no
                // operator pin can apply to it and the requirement has
                // nothing to bind to. The override path is exempt by
                // construction, not by an escape hatch: the operator
                // named this artifact on the command line, which is the
                // same authorization the pin records (ADR-0025).
                require_component_digest: false,
                ..supervisor::BootEnv::from_config(engine_cfg)
            };
            let id = wasm
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module".to_owned());
            let mut entry = ModuleEntry::new(id, wasm);
            entry.manifest = manifest;
            Supervisor::boot_single(
                &engine,
                &linker,
                &entry,
                &components,
                &env,
                &extensions,
                clocks,
            )
            .await?
        } else if !engine_cfg.modules.is_empty() {
            Supervisor::boot(
                &engine,
                &linker,
                engine_cfg,
                &components,
                &extensions,
                clocks,
            )
            .await?
        } else {
            return Err(refuse_launch(LaunchRefusal::NothingToRun));
        };

        let alive = supervisor.alive_count();
        let plan = supervisor.source_plan();
        info!(
            modules = supervisor.module_count(),
            alive,
            chains = plan.block_chains.len(),
            // The drain default tracks `deadline_secs`, so a systemd
            // `TimeoutStopSec` sized against an older deadline is silently
            // too small. Emit the resolved value so it can be compared.
            shutdown_drain_secs = engine_cfg.limits.shutdown_drain.as_secs(),
            "supervisor ready"
        );
        if alive == 0 {
            let modules = supervisor.module_count();
            return Err(refuse_launch(if wasm_override {
                LaunchRefusal::AllDeadOverride { modules }
            } else {
                LaunchRefusal::AllDeadConfigured { modules }
            }));
        }

        // The OS signal listener: SIGINT/SIGTERM ends it, and its end (or
        // panic) fires shutdown via the critical-task path. It also awaits
        // shutdown itself so a programmatic shutdown or a handle drop winds
        // it down rather than leaking it.
        let executor = tasks.executor();
        let mut listener_signal = tasks.subscribe();
        let mut fallback_signal = tasks.subscribe();
        executor.spawn_critical("os-signal-listener", async move {
            tokio::select! {
                res = event_loop::wait_for_os_signal() => match res {
                    Ok(name) => info!(signal = %name, "OS signal received, shutting down"),
                    Err(err) => {
                        warn!(error = %err, "signal handler failed - programmatic shutdown only");
                        fallback_signal.recv().await;
                    }
                },
                () = listener_signal.recv() => {}
            }
        });

        // The handle keeps the log read side reachable after launch consumes
        // the components.
        let logs = components.logs.clone();
        // Extension sources open only for trigger kinds some live module
        // declares; an extension returns no stream when it has nothing to
        // observe.
        let mut reconnect_tasks = TaskSet::new();
        let mut extension_streams = Vec::new();
        {
            let mut sources = SourceContext::new(
                engine_cfg,
                &plan.demanded_extension_kinds,
                &executor,
                &mut reconnect_tasks,
            );
            for ext in &extensions {
                extension_streams.extend(ext.open_sources(&mut sources)?);
            }
        }

        match plan.viability(extension_streams.len()) {
            Viability::DeadHoldTriggers => {
                return Err(refuse_launch(LaunchRefusal::DeadHoldTriggers));
            }
            Viability::Nothing => {
                // Nothing to drive: return a handle whose event loop is
                // already complete so `wait` resolves immediately.
                info!("no [[trigger]] entries - engine has nothing to run; exiting");
                let event_loop = executor.spawn(async { RunEnd::NothingLive });
                return Ok(RuntimeHandle {
                    event_loop,
                    tasks,
                    logs,
                    drain_timeout: engine_cfg.limits.shutdown_drain,
                    _add_ons: add_on_handles,
                });
            }
            Viability::Live => {}
        }

        // Open per-chain block streams + per-module chain-log streams
        // through the executor, then drive them in the event loop until
        // shutdown.
        let block_streams = event_loop::open_block_streams(
            &components.chain,
            &plan.block_chains,
            &executor,
            &mut reconnect_tasks,
        );
        let chain_log_streams = event_loop::open_chain_log_streams(
            &components.chain,
            plan.event_sources,
            &executor,
            &mut reconnect_tasks,
        );
        // The event-loop task holds the graceful guard until `run` returns
        // (after its final dispatch and cursor commit); shutdown ends the
        // loop between guest calls rather than cancelling it, so the drain
        // blocks on at most one deadline-bounded call.
        let stop = tasks.subscribe();
        let event_loop = executor.spawn_graceful(move |graceful| async move {
            let mut supervisor = supervisor; // rebind as mut: the dispatch calls below take &mut self
            supervisor.stop_on(stop);
            let outcome = event_loop::run(
                &mut supervisor,
                block_streams,
                chain_log_streams,
                extension_streams,
                reconnect_tasks,
                graceful.into_future(),
            )
            .await;
            if matches!(outcome.end, RunEnd::Shutdown | RunEnd::NothingLive) {
                info!("done");
            }
            outcome.end
        });

        Ok(RuntimeHandle {
            event_loop,
            tasks,
            logs,
            drain_timeout: engine_cfg.limits.shutdown_drain,
            _add_ons: add_on_handles,
        })
    }
}

/// Opens the backends with a fresh [`TaskManager`], then drives
/// [`AssembledRuntime::launch`]; the shared tail of every terminal stage.
async fn open_and_launch<T, C, S, L>(
    config: &EngineConfig,
    extensions: Vec<Arc<dyn Extension<T>>>,
    add_ons: AddOns,
    wasm: Option<PathBuf>,
    manifest: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
    components: ComponentsBuilder<C, S, L>,
) -> Result<RuntimeHandle, RuntimeError>
where
    T: RuntimeTypes<State = HostState<T>>,
    C: ComponentBuilder<Output = ProviderPool>,
    S: ComponentBuilder<Output = T::Store>,
    L: ComponentBuilder<Output = LogPipeline>,
{
    let tasks = TaskManager::new();
    let executor = tasks.executor();
    let data_dir = config.engine.state_dir.clone();
    let build_ctx = BuilderContext {
        config,
        data_dir: &data_dir,
        executor: &executor,
    };
    let components = components.build::<T>(&build_ctx).await?;

    let runtime = AssembledRuntime {
        components,
        extensions,
        add_ons,
        wasm,
        manifest,
        clocks,
    };
    runtime.launch(LaunchContext { tasks, config }).await
}

/// Entry stage of the type-state runtime builder: only the config is bound.
pub struct RuntimeBuilder<'a> {
    config: &'a EngineConfig,
}

impl<'a> RuntimeBuilder<'a> {
    /// Start a builder over a loaded config.
    pub fn new(config: &'a EngineConfig) -> Self {
        Self { config }
    }

    /// Bind the [`RuntimeTypes`] lattice.
    #[must_use]
    pub fn with_types<T: RuntimeTypes>(self) -> TypedBuilder<'a, T> {
        TypedBuilder {
            config: self.config,
            extensions: Vec::new(),
            wasm: None,
            manifest: None,
            clocks: None,
            _t: PhantomData,
        }
    }

    /// Bind a `Default` [`Runtime`] preset by marker; sugar over
    /// [`with_runtime`](Self::with_runtime).
    #[must_use]
    pub fn runtime<R: Runtime + Default>(self) -> PresetBuilder<'a, R> {
        self.with_runtime(R::default())
    }

    /// Bind a [`Runtime`] preset by value, carrying pre-built backends into
    /// the launch.
    #[must_use]
    pub fn with_runtime<R: Runtime>(self, preset: R) -> PresetBuilder<'a, R> {
        PresetBuilder {
            config: self.config,
            preset,
            extensions: Vec::new(),
            wasm: None,
            manifest: None,
            clocks: None,
        }
    }
}

/// Terminal stage of the preset shortcut, leaving only optional extension
/// hooks and the module source before [`launch`](Self::launch).
pub struct PresetBuilder<'a, R: Runtime> {
    config: &'a EngineConfig,
    preset: R,
    extensions: Vec<Arc<dyn Extension<R::Types>>>,
    wasm: Option<PathBuf>,
    manifest: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
}

impl<'a, R: Runtime> PresetBuilder<'a, R> {
    /// Append extensions on top of the preset's own.
    #[must_use]
    pub fn with_extensions(
        mut self,
        extensions: impl IntoIterator<Item = Arc<dyn Extension<R::Types>>>,
    ) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Set the single-module source override, taking precedence over engine.toml
    /// `[[modules]]`. Both `None` runs the configured modules.
    #[must_use]
    pub fn with_module_source(mut self, wasm: Option<PathBuf>, manifest: Option<PathBuf>) -> Self {
        self.wasm = wasm;
        self.manifest = manifest;
        self
    }

    /// Override the per-store WASI wall and monotonic clocks, including stores
    /// rebuilt on restart. Omitting it leaves the ambient host clocks.
    #[must_use]
    pub fn with_wasi_clocks(mut self, clocks: WasiClockOverride) -> Self {
        self.clocks = Some(clocks);
        self
    }

    /// Override the preset's component builders before launch; `map` swaps one
    /// seam while the preset's extensions and add-ons carry through. Mirror of
    /// [`TypedBuilder::with_components`].
    #[must_use]
    pub fn with_components<C, S, L>(
        self,
        map: impl FnOnce(
            ComponentsBuilder<R::ChainBuilder, R::StoreBuilder, R::LogsBuilder>,
        ) -> ComponentsBuilder<C, S, L>,
    ) -> PresetComponentsBuilder<'a, R::Types, C, S, L> {
        // Gather the preset's extensions and add-ons before `components`
        // consumes the preset by value.
        let mut extensions = self.preset.extensions(self.config);
        extensions.extend(self.extensions);
        let add_ons = self.preset.add_ons();
        let components = map(self.preset.components());
        PresetComponentsBuilder {
            config: self.config,
            extensions,
            add_ons,
            wasm: self.wasm,
            manifest: self.manifest,
            clocks: self.clocks,
            components,
        }
    }

    /// Open the preset's backends and launch, driving
    /// [`AssembledRuntime::launch`] with a fresh [`TaskManager`].
    pub async fn launch(self) -> Result<RuntimeHandle, RuntimeError> {
        let Self {
            config,
            preset,
            extensions: appended,
            wasm,
            manifest,
            clocks,
        } = self;
        let mut extensions = preset.extensions(config);
        extensions.extend(appended);
        let add_ons = preset.add_ons();
        open_and_launch(
            config,
            extensions,
            add_ons,
            wasm,
            manifest,
            clocks,
            preset.components(),
        )
        .await
    }
}

/// A preset with its component builders overridden through
/// [`PresetBuilder::with_components`], leaving only [`launch`](Self::launch).
pub struct PresetComponentsBuilder<'a, T: RuntimeTypes, C, S, L> {
    config: &'a EngineConfig,
    extensions: Vec<Arc<dyn Extension<T>>>,
    add_ons: AddOns,
    wasm: Option<PathBuf>,
    manifest: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
    components: ComponentsBuilder<C, S, L>,
}

impl<T, C, S, L> PresetComponentsBuilder<'_, T, C, S, L>
where
    T: RuntimeTypes<State = HostState<T>>,
    C: ComponentBuilder<Output = ProviderPool>,
    S: ComponentBuilder<Output = T::Store>,
    L: ComponentBuilder<Output = LogPipeline>,
{
    /// Open the overridden backends and launch, otherwise as
    /// [`PresetBuilder::launch`].
    pub async fn launch(self) -> Result<RuntimeHandle, RuntimeError> {
        open_and_launch(
            self.config,
            self.extensions,
            self.add_ons,
            self.wasm,
            self.manifest,
            self.clocks,
            self.components,
        )
        .await
    }
}

/// The lattice is bound; extensions and an optional module-source override
/// may be added before the component builders.
pub struct TypedBuilder<'a, T: RuntimeTypes> {
    config: &'a EngineConfig,
    extensions: Vec<Arc<dyn Extension<T>>>,
    wasm: Option<PathBuf>,
    manifest: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
    _t: PhantomData<fn() -> T>,
}

impl<'a, T: RuntimeTypes> TypedBuilder<'a, T> {
    /// Add the extensions.
    #[must_use]
    pub fn with_extensions(
        mut self,
        extensions: impl IntoIterator<Item = Arc<dyn Extension<T>>>,
    ) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Set the single-module source override, taking precedence over engine.toml
    /// `[[modules]]`. Both `None` runs the configured modules.
    #[must_use]
    pub fn with_module_source(mut self, wasm: Option<PathBuf>, manifest: Option<PathBuf>) -> Self {
        self.wasm = wasm;
        self.manifest = manifest;
        self
    }

    /// Override the per-store WASI wall and monotonic clocks, including stores
    /// rebuilt on restart. Omitting it leaves the ambient host clocks.
    #[must_use]
    pub fn with_wasi_clocks(mut self, clocks: WasiClockOverride) -> Self {
        self.clocks = Some(clocks);
        self
    }

    /// Bind the component builders that open the backends at launch.
    #[must_use]
    pub fn with_components<C, S, L>(
        self,
        components: ComponentsBuilder<C, S, L>,
    ) -> ReadyBuilder<'a, T, C, S, L> {
        ReadyBuilder {
            config: self.config,
            extensions: self.extensions,
            wasm: self.wasm,
            manifest: self.manifest,
            clocks: self.clocks,
            components,
            add_ons: AddOns::new(),
        }
    }
}

/// The assembly is complete; [`launch`](Self::launch) opens the backends and
/// runs.
pub struct ReadyBuilder<'a, T: RuntimeTypes, C, S, L> {
    config: &'a EngineConfig,
    extensions: Vec<Arc<dyn Extension<T>>>,
    wasm: Option<PathBuf>,
    manifest: Option<PathBuf>,
    clocks: Option<WasiClockOverride>,
    components: ComponentsBuilder<C, S, L>,
    add_ons: AddOns,
}

impl<T: RuntimeTypes, C, S, L> ReadyBuilder<'_, T, C, S, L> {
    /// Bind the cross-cutting add-on set installed before the engine boots;
    /// defaults to none.
    #[must_use]
    pub fn with_add_ons(mut self, add_ons: AddOns) -> Self {
        self.add_ons = add_ons;
        self
    }
}

impl<T, C, S, L> ReadyBuilder<'_, T, C, S, L>
where
    T: RuntimeTypes<State = HostState<T>>,
    C: ComponentBuilder<Output = ProviderPool>,
    S: ComponentBuilder<Output = T::Store>,
    L: ComponentBuilder<Output = LogPipeline>,
{
    /// Open the backends and launch, driving [`AssembledRuntime::launch`]
    /// with a fresh [`TaskManager`].
    pub async fn launch(self) -> Result<RuntimeHandle, RuntimeError> {
        open_and_launch(
            self.config,
            self.extensions,
            self.add_ons,
            self.wasm,
            self.manifest,
            self.clocks,
            self.components,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::addons::{AddOns, RuntimeAddOn};
    use crate::engine_config::EngineConfig;
    use crate::error::BootRefusal;
    use crate::manifest::NamespaceCaps;
    use crate::preset::{CoreRuntime, Runtime as RuntimePreset};
    use crate::test_utils::ManualClock;
    use crate::test_utils::workspace_root;
    use crate::test_utils::{
        Prebuilt, Refusal, TestManifest, example_wasm_or_skip, module_wasm_or_skip,
    };
    use nexum_runtime_api::{ExtensionError, HostWallClock};
    use nexum_runtime_chain::ProviderPoolBuilder;
    use nexum_runtime_logs::LogPipelineBuilder;
    use nexum_runtime_store::LocalStoreBuilder;
    use nexum_runtime_wasm::HostState;
    use wasmtime::component::Linker;

    /// The preset shortcut reaches the supervisor boot, which bails on the
    /// default config's empty module set.
    #[tokio::test]
    async fn preset_launch_runs_the_build_path_then_bails_without_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let err = match RuntimeBuilder::new(&config)
            .runtime::<CoreRuntime>()
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));
    }

    #[tokio::test]
    async fn an_embedder_matches_a_boot_refusal_without_downcasting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let err = match RuntimeBuilder::new(&config)
            .runtime::<CoreRuntime>()
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        match err {
            crate::error::RuntimeError::Launch(LaunchRefusal::NothingToRun) => {}
            other => panic!("expected the typed NothingToRun arm, got: {other}"),
        }
    }

    /// Every launch refusal site routes through [`refuse_launch`], so this
    /// pins the emitted name, the label key, and the increment for the
    /// launch classes; the wait-time event-loop failure counts nothing.
    #[test]
    fn launch_refusals_count_under_the_boot_refusal_counter() {
        use crate::test_utils::metrics_util::debugging::DebugValue;
        use crate::test_utils::{capture_metrics, samples_named};

        let (err, samples) = capture_metrics(|| refuse_launch(LaunchRefusal::NothingToRun));
        let hits = samples_named(&samples, "nexum_runtime_boot_refusals_total");
        assert_eq!(hits.len(), 1, "one series: {samples:?}");
        assert!(
            hits[0].has_label("error_kind", "nothing_to_run"),
            "{:?}",
            hits[0].labels,
        );
        assert!(
            matches!(hits[0].value, DebugValue::Counter(1)),
            "{:?}",
            hits[0].value,
        );
        assert!(
            matches!(
                err,
                crate::error::RuntimeError::Launch(LaunchRefusal::NothingToRun)
            ),
            "the returned value is the RuntimeError an embedder matches on",
        );

        let (_, samples) = capture_metrics(|| refuse_launch(LaunchRefusal::EventLoopGone));
        assert!(
            samples_named(&samples, "nexum_runtime_boot_refusals_total").is_empty(),
            "a wait-time failure is not a boot refusal: {samples:?}",
        );
    }

    /// Counts linker hook runs.
    struct CountingExt {
        namespace: &'static str,
        prefix: &'static str,
        linked: Arc<AtomicUsize>,
    }

    impl Extension<CoreRuntime> for CountingExt {
        fn namespace(&self) -> &'static str {
            self.namespace
        }
        fn capabilities(&self) -> NamespaceCaps {
            NamespaceCaps {
                prefix: self.prefix,
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<CoreRuntime>>) -> Result<(), ExtensionError> {
            self.linked.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A value-bound preset carrying its own extension.
    struct ExtPreset {
        linked: Arc<AtomicUsize>,
    }

    impl crate::sealed::SealedRuntime for ExtPreset {}

    impl RuntimePreset for ExtPreset {
        type Types = CoreRuntime;
        type ChainBuilder = ProviderPoolBuilder;
        type StoreBuilder = LocalStoreBuilder;
        type LogsBuilder = LogPipelineBuilder;

        fn components(self) -> ComponentsBuilder<ProviderPoolBuilder, LocalStoreBuilder> {
            ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
        }

        fn add_ons(&self) -> AddOns {
            Vec::new()
        }

        fn extensions(&self, _config: &EngineConfig) -> Vec<Arc<dyn Extension<CoreRuntime>>> {
            vec![Arc::new(CountingExt {
                namespace: "alpha",
                prefix: "alpha:ext/",
                linked: self.linked.clone(),
            })]
        }
    }

    /// Preset extensions and appended extensions each link exactly once.
    #[tokio::test]
    async fn preset_extensions_and_appended_extensions_both_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let preset_linked = Arc::new(AtomicUsize::new(0));
        let appended_linked = Arc::new(AtomicUsize::new(0));
        let appended: Arc<dyn Extension<CoreRuntime>> = Arc::new(CountingExt {
            namespace: "beta",
            prefix: "beta:ext/",
            linked: appended_linked.clone(),
        });

        let err = match RuntimeBuilder::new(&config)
            .with_runtime(ExtPreset {
                linked: preset_linked.clone(),
            })
            .with_extensions([appended])
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));
        assert_eq!(preset_linked.load(Ordering::SeqCst), 1, "preset extension");
        assert_eq!(
            appended_linked.load(Ordering::SeqCst),
            1,
            "appended extension"
        );
    }

    /// Captures the wall clock handed through the extension seam.
    struct ClockCaptureExt {
        seen: Arc<OnceLock<Arc<dyn HostWallClock + Send + Sync>>>,
    }

    impl Extension<CoreRuntime> for ClockCaptureExt {
        fn namespace(&self) -> &'static str {
            "clockcap"
        }
        fn capabilities(&self) -> NamespaceCaps {
            NamespaceCaps {
                prefix: "clockcap:ext/",
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<CoreRuntime>>) -> Result<(), ExtensionError> {
            Ok(())
        }
        fn attach_clock(&self, wall: Arc<dyn HostWallClock + Send + Sync>) {
            let _ = self.seen.set(wall);
        }
    }

    /// Launch expecting the empty-module-set bail; the clock attach runs
    /// before the boot that bails.
    async fn launch_capturing_clock(
        clocks: Option<WasiClockOverride>,
    ) -> Arc<dyn HostWallClock + Send + Sync> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let seen = Arc::new(OnceLock::new());
        let ext: Arc<dyn Extension<CoreRuntime>> = Arc::new(ClockCaptureExt { seen: seen.clone() });
        let mut builder = RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_extensions([ext]);
        if let Some(clocks) = clocks {
            builder = builder.with_wasi_clocks(clocks);
        }
        let err = match builder
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));
        seen.get().expect("clock attached before boot").clone()
    }

    /// The override's wall clock and the extension's attached clock read one
    /// timeline.
    #[tokio::test]
    async fn extension_clock_follows_the_wasi_override() {
        let clock = ManualClock::new();
        clock.set(UNIX_EPOCH + Duration::from_secs(1_000));

        let wall = launch_capturing_clock(Some(clock.as_override())).await;
        assert_eq!(wall.now(), Duration::from_secs(1_000));

        clock.advance(Duration::from_secs(50));
        assert_eq!(
            wall.now(),
            Duration::from_secs(1_050),
            "override and extension share one timeline",
        );
    }

    /// Without an override the extension receives the real host clock.
    #[tokio::test]
    async fn extension_clock_defaults_to_the_real_clock() {
        let wall = launch_capturing_clock(None).await;
        let host = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("host time is past the epoch");
        assert!(
            wall.now().abs_diff(host) < Duration::from_secs(60),
            "attached clock tracks host wall time",
        );
    }

    /// A value-bound preset handing back an already-built backend.
    struct PrebuiltLogsPreset {
        logs: LogPipeline,
    }

    impl crate::sealed::SealedRuntime for PrebuiltLogsPreset {}

    impl RuntimePreset for PrebuiltLogsPreset {
        type Types = CoreRuntime;
        type ChainBuilder = ProviderPoolBuilder;
        type StoreBuilder = LocalStoreBuilder;
        type LogsBuilder = Prebuilt<LogPipeline>;

        fn components(
            self,
        ) -> ComponentsBuilder<ProviderPoolBuilder, LocalStoreBuilder, Prebuilt<LogPipeline>>
        {
            ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
                .with_logs(Prebuilt(self.logs))
        }

        fn add_ons(&self) -> AddOns {
            Vec::new()
        }
    }

    /// A preset hands a pre-built pipeline through into the built bundle.
    #[tokio::test]
    async fn preset_hands_over_a_prebuilt_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = EngineConfig::default();
        let tasks = TaskManager::new();
        let executor = tasks.executor();
        let build_ctx = BuilderContext {
            config: &config,
            data_dir: dir.path(),
            executor: &executor,
        };

        let custom = LogPipeline::in_memory(config.limits.logs);
        let components = PrebuiltLogsPreset {
            logs: custom.clone(),
        }
        .components()
        .build::<CoreRuntime>(&build_ctx)
        .await
        .expect("build from the preset's builders");

        assert!(
            Arc::ptr_eq(&components.logs.router(), &custom.router()),
            "bundle carries the preset's pre-built pipeline",
        );
    }

    /// A core-lattice preset with no add-ons, avoiding the process-global
    /// Prometheus recorder (only one install succeeds per process).
    struct NoAddOnCore;

    impl crate::sealed::SealedRuntime for NoAddOnCore {}

    impl RuntimePreset for NoAddOnCore {
        type Types = CoreRuntime;
        type ChainBuilder = ProviderPoolBuilder;
        type StoreBuilder = LocalStoreBuilder;
        type LogsBuilder = LogPipelineBuilder;

        fn components(self) -> ComponentsBuilder<ProviderPoolBuilder, LocalStoreBuilder> {
            ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
        }

        fn add_ons(&self) -> AddOns {
            Vec::new()
        }
    }

    /// Counts builds.
    struct CountingLogsBuilder(Arc<AtomicUsize>);

    impl ComponentBuilder for CountingLogsBuilder {
        type Output = LogPipeline;
        async fn build(
            self,
            ctx: &BuilderContext<'_>,
        ) -> Result<LogPipeline, crate::error::BoxError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(LogPipeline::in_memory(ctx.config.limits.logs))
        }
    }

    /// `with_components` overrides a seam: the substituted logs builder runs
    /// once, then the launch bails on the empty module set.
    #[tokio::test]
    async fn preset_with_components_overrides_a_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let built = Arc::new(AtomicUsize::new(0));
        let seen = built.clone();
        let err = match RuntimeBuilder::new(&config)
            .with_runtime(NoAddOnCore)
            .with_components(move |c| c.with_logs(CountingLogsBuilder(seen)))
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));
        assert_eq!(
            built.load(Ordering::SeqCst),
            1,
            "overridden logs builder ran once",
        );
    }

    /// Full preset-path launch with an overridden logs seam; skips when the
    /// module fixture is not built (`just build-module`).
    #[tokio::test]
    async fn e2e_preset_with_components_launches_through_overridden_logs() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let manifest = workspace_root().join("modules/example/component.toml");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");

        let custom = LogPipeline::in_memory(config.limits.logs);
        let mut handle = RuntimeBuilder::new(&config)
            .with_runtime(NoAddOnCore)
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(|c| c.with_logs(Prebuilt(custom.clone())))
            .launch()
            .await
            .expect("launch through the overridden logs seam");

        assert!(
            Arc::ptr_eq(&handle.logs().router(), &custom.router()),
            "run reads the overridden pipeline",
        );

        handle.shutdown();
        handle.wait().await.expect("clean shutdown");
    }

    /// A `[policy.component]` row keyed to an id equal to the override's
    /// file stem must not bind: the stem is author-controlled, so the
    /// override gets the `[policy]` defaults (ADR-0018). The row here
    /// excludes `logging`, which the example manifest declares, so the
    /// launch only succeeds when the row goes unapplied.
    #[tokio::test]
    async fn a_module_source_override_never_binds_a_policy_component_row() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let stem = wasm
            .file_stem()
            .expect("fixture has a file stem")
            .to_string_lossy()
            .into_owned();
        let manifest = workspace_root().join("modules/example/component.toml");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        config.modules.push(ModuleEntry::new(
            stem.clone(),
            dir.path().join("unrelated.wasm"),
        ));
        config.policy.component.insert(
            stem,
            crate::engine_config::ComponentPolicy {
                capabilities: Some(vec!["chain".to_owned()]),
                ..Default::default()
            },
        );

        let mut handle = RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
            .expect("the row must not bind to the override");
        handle.shutdown();
        handle.wait().await.expect("clean shutdown");
    }

    /// The single-wasm override is exempt from the operator-pin
    /// requirement by construction: no `[[modules]]` entry describes it,
    /// so no pin can apply to it (ADR-0025). The config here is the
    /// defaulted, strict one, and the artifact is not a component, so the
    /// launch must fail at compile rather than at the digest gate.
    #[tokio::test]
    async fn a_module_source_override_is_exempt_from_the_operator_pin_requirement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wasm = dir.path().join("override.wasm");
        std::fs::write(&wasm, b"not a component").expect("write artifact");
        let manifest = TestManifest::new("override")
            .cap("logging")
            .write_to(dir.path());

        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        assert!(
            config.engine.require_component_digest,
            "the exemption only means anything under the strict default",
        );

        let err = match RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("an artifact that is not a component must not launch"),
            Err(err) => err,
        };
        Refusal::from(err)
            .names("compile")
            .lacks("carries no digest");
    }

    /// The mirror of the exemption, and the point of the default: a
    /// configured `[[modules]]` entry with no `digest` refuses over an
    /// `EngineConfig` nobody touched, before any compile (ADR-0025). The
    /// scenario suite pins the gate under an explicitly set flag, so this
    /// is the only place the defaulted value reaches a boot.
    #[tokio::test]
    async fn a_configured_entry_without_a_digest_refuses_under_the_default_config() {
        use crate::error::LoadRefusal;

        let dir = tempfile::tempdir().expect("tempdir");
        let wasm = dir.path().join("configured.wasm");
        std::fs::write(&wasm, b"not a component").expect("write artifact");
        let manifest = TestManifest::new("configured")
            .cap("logging")
            .write_to(dir.path());

        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        let mut entry = ModuleEntry::new("configured", wasm);
        entry.manifest = Some(manifest);
        config.modules.push(entry);

        let err = match RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("an unpinned entry must not launch under the default"),
            Err(err) => err,
        };
        Refusal::from(err)
            .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
            .lacks("compile");
    }

    /// Every module failing `init` aborts launch instead of idling.
    #[tokio::test]
    async fn launch_bails_when_all_modules_fail_init() {
        let Some(wasm) = module_wasm_or_skip("price-alert") else {
            return;
        };

        let dir = tempfile::tempdir().expect("tempdir");
        // Unparseable threshold: the module loads, then `init` fails.
        let manifest = TestManifest::new("price-alert")
            .cap("logging")
            .cap("chain")
            .block_trigger(11_155_111)
            .config(
                "oracle_address",
                "0x694AA1769357215DE4FAC081bf1f309aDC325306",
            )
            .config("decimals", "8")
            .config("threshold", "not-a-number")
            .config("direction", "below")
            .config("every_n_blocks", "1")
            .write_to(dir.path());

        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        // The chain gate must admit the module; init failure is the asserted path.
        config.chains = crate::test_utils::test_chain_configs();

        let err = match RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("init-failing module must abort launch"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| {
            matches!(e, LaunchRefusal::AllDeadOverride { modules: 1 })
        });
    }

    #[tokio::test]
    async fn launch_bails_on_an_unconfigured_chain_trigger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wasm = dir.path().join("missing.wasm");
        let manifest = dir.path().join("component.toml");
        std::fs::write(
            &manifest,
            "[component]\nname = \"example\"\n\n[dependencies]\nlogging = {}\n\n\
             [[trigger]]\non = \"block\"\nchain_id = 424242\n",
        )
        .expect("write manifest");

        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        config.chains = crate::test_utils::test_chain_configs();

        let err = match RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("an unconfigured chain trigger must abort launch"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChain { name, chain_id: 424_242, .. }
                if name == "example")
        });
    }

    /// Add-ons install before the supervisor boots, exactly once.
    #[tokio::test]
    async fn assembled_runtime_installs_add_ons_before_boot() {
        struct CountingAddOn(Arc<AtomicUsize>);
        impl RuntimeAddOn for CountingAddOn {
            fn install(
                &self,
                _ctx: &AddOnsContext<'_>,
            ) -> Result<AddOnHandle, crate::error::BoxError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(AddOnHandle::named("counting"))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("state");
        let mut config = EngineConfig::default();
        config.engine.state_dir = data_dir.clone();

        let tasks = TaskManager::new();
        let executor = tasks.executor();
        let build_ctx = BuilderContext {
            config: &config,
            data_dir: &data_dir,
            executor: &executor,
        };
        let components = ComponentsBuilder::new(ProviderPoolBuilder, LocalStoreBuilder)
            .build::<CoreRuntime>(&build_ctx)
            .await
            .expect("build core components");

        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = AssembledRuntime {
            components,
            extensions: Vec::new(),
            add_ons: vec![Box::new(CountingAddOn(calls.clone()))],
            wasm: None,
            manifest: None,
            clocks: None,
        };
        let ctx = LaunchContext {
            tasks,
            config: &config,
        };

        let err = match runtime.launch(ctx).await {
            Ok(_) => panic!("no modules configured; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "add-on installed once, before the boot that bails",
        );
    }

    /// Full builder-path launch against the example module; skips when the
    /// fixture is not built (`just build-module`).
    #[tokio::test]
    async fn e2e_builder_launch_exposes_logs_and_stops_on_shutdown() {
        let Some(wasm) = example_wasm_or_skip() else {
            return;
        };
        let manifest = workspace_root().join("modules/example/component.toml");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = EngineConfig::default();
        config.engine.state_dir = dir.path().join("state");
        // Non-default, so a launch path regressing to a constant fails here.
        config.limits.shutdown_drain = Duration::from_secs(9);

        let mut handle = RuntimeBuilder::new(&config)
            .with_types::<CoreRuntime>()
            .with_module_source(Some(wasm), Some(manifest))
            .with_components(ComponentsBuilder::new(
                ProviderPoolBuilder,
                LocalStoreBuilder,
            ))
            .launch()
            .await
            .expect("launch the example module");

        // The handle carries the run/log read side of the launched pipeline.
        let logs = handle.logs().clone();
        let _ = logs.list_runs("example");

        assert_eq!(
            handle.drain_timeout,
            Duration::from_secs(9),
            "the configured drain reaches the handle `wait` reads",
        );

        handle.shutdown();
        handle.wait().await.expect("clean shutdown");
    }

    fn handle_over(tasks: TaskManager, event_loop: TaskHandle<RunEnd>) -> RuntimeHandle {
        RuntimeHandle {
            event_loop,
            tasks,
            logs: test_logs(),
            drain_timeout: EngineConfig::default().limits.shutdown_drain,
            _add_ons: Vec::new(),
        }
    }

    fn test_logs() -> LogPipeline {
        LogPipeline::in_memory(EngineConfig::default().limits.logs)
    }

    /// A cleanly completing event loop resolves `wait` to `Ok`.
    #[tokio::test]
    async fn runtime_handle_wait_is_ok_on_clean_completion() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn(async { RunEnd::Shutdown });
        handle_over(tasks, event_loop)
            .wait()
            .await
            .expect("clean completion resolves Ok");
    }

    /// Firing the shutdown signal drives the loop to completion and `wait`
    /// returns.
    #[tokio::test]
    async fn runtime_handle_shutdown_signal_drives_wait_to_return() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn_graceful(|graceful| async move {
            drop(graceful.await);
            RunEnd::Shutdown
        });
        let mut handle = handle_over(tasks, event_loop);
        handle.shutdown();
        handle.wait().await.expect("wait returns after the signal");
    }

    /// A terminal source exit surfaces a non-zero result carrying the reason.
    #[tokio::test]
    async fn runtime_handle_wait_is_err_on_a_terminal_source_exit() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn(async {
            RunEnd::SourceTerminal(nexum_tasks::SourceTermination {
                module: None,
                chain_id: 7,
                reason: "endpoint no longer serves chain 7".to_owned(),
            })
        });
        let err = handle_over(tasks, event_loop)
            .wait()
            .await
            .expect_err("a terminal source exit is not a clean stop");
        assert!(
            err.to_string()
                .contains("endpoint no longer serves chain 7"),
            "the operator sees the source's reason: {err}",
        );
        Refusal::from(err).variant::<LaunchRefusal>(|e| {
            matches!(e, LaunchRefusal::SourceTerminal { chain_id: 7, .. })
        });
    }

    /// An abnormally-stopped event loop surfaces an error from `wait`.
    #[tokio::test]
    async fn runtime_handle_wait_is_err_on_abnormal_stop() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn(async {
            std::future::pending::<()>().await;
            RunEnd::Shutdown
        });
        event_loop.abort();
        let err = handle_over(tasks, event_loop)
            .wait()
            .await
            .expect_err("aborted task surfaces an error");
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::EventLoopGone));
    }

    /// An unaccounted stream end means a dead pump, so it exits non-zero for
    /// the `Restart=on-failure` unit the loop's own warning asks for.
    #[tokio::test]
    async fn runtime_handle_wait_is_err_on_an_unexpected_stream_end() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn(async { RunEnd::StreamEnded });
        let err = handle_over(tasks, event_loop)
            .wait()
            .await
            .expect_err("an unexpected task end is not a clean stop");
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::EventLoopGone));
    }

    /// Every source ending terminally with nothing else declared is the
    /// deliberate quiet stop, so it must stay a zero exit.
    #[tokio::test]
    async fn runtime_handle_wait_is_ok_when_nothing_is_live() {
        let tasks = TaskManager::new();
        let event_loop = tasks.executor().spawn(async { RunEnd::NothingLive });
        handle_over(tasks, event_loop)
            .wait()
            .await
            .expect("a run with nothing left to do stops cleanly");
    }

    /// Dropping the handle without `wait` still drains the event loop.
    #[tokio::test]
    async fn dropping_handle_without_wait_drains_the_event_loop() {
        let tasks = TaskManager::new();
        let drained = Arc::new(AtomicUsize::new(0));
        let seen = drained.clone();
        let event_loop = tasks.executor().spawn_graceful(move |graceful| async move {
            let guard = graceful.await;
            seen.fetch_add(1, Ordering::SeqCst);
            drop(guard);
            RunEnd::Shutdown
        });
        let handle = handle_over(tasks, event_loop);

        drop(handle);

        for _ in 0..200 {
            if drained.load(Ordering::SeqCst) == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("event loop did not drain after the handle was dropped");
    }
}
