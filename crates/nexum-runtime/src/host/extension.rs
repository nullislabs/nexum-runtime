//! Extension seam: what one extension contributes to the host (namespace,
//! capabilities, linker hook, trigger sources, and manifest-section install
//! predicates).

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use nexum_tasks::{TaskExecutor, TaskExit, TaskSet};
use wasmtime::component::Linker;
pub use wasmtime_wasi::HostWallClock;

use crate::bindings::nexum::host::types::Trigger;
use crate::engine_config::EngineConfig;
use crate::host::component::RuntimeTypes;
use crate::host::state::HostState;
use crate::manifest::{ExtensionSections, NamespaceCaps};
use crate::supervisor::WasiClockOverride;

/// One runtime extension; a module importing its interface boots only if both
/// the linker entry and the capability namespace are registered.
pub trait Extension<T: RuntimeTypes>: Send + Sync + 'static {
    /// Namespace this extension owns.
    fn namespace(&self) -> &'static str;

    /// Capability namespace merged into enforcement.
    fn capabilities(&self) -> NamespaceCaps;

    /// Add the extension's imports to a worker linker, after core interfaces
    /// and before instantiation.
    fn link(&self, linker: &mut Linker<HostState<T>>) -> anyhow::Result<()>;

    /// The effective host wall clock, handed once per launch before
    /// [`link`](Self::link): the WASI override's wall clock when set, else real.
    fn attach_clock(&self, wall: Arc<dyn HostWallClock + Send + Sync>) {
        let _ = wall;
    }

    /// Manifest section names this extension claims; an unclaimed non-core
    /// section is refused at boot.
    fn manifest_sections(&self) -> &'static [&'static str] {
        &[]
    }

    /// Admit one worker at install over its manifest sections; `Err`
    /// refuses fail-fast.
    fn admit_worker(&self, worker: &str, sections: &ExtensionSections) -> anyhow::Result<()> {
        let _ = (worker, sections);
        Ok(())
    }

    /// Trigger kinds this extension's sources emit; an unknown non-core
    /// kind is refused at boot.
    fn emits_trigger_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Open the extension's sources after boot; the event loop merges
    /// and dispatches them.
    fn open_sources(
        &self,
        sources: &mut SourceContext<'_>,
    ) -> anyhow::Result<Vec<ExtensionSource>> {
        let _ = sources;
        Ok(Vec::new())
    }
}

/// Hand every extension the effective wall clock. Every launch path calls
/// this before it builds the linker.
pub(crate) fn attach_wall_clock<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<&WasiClockOverride>,
) {
    let wall = WasiClockOverride::effective_wall(clocks);
    for ext in extensions {
        ext.attach_clock(wall.clone());
    }
}

/// Delivered to every module with a `[[trigger]]` of `extension_kind`
/// whose filters match `attrs`.
pub struct ExtensionDelivery {
    /// Manifest trigger kind that routes this delivery.
    pub extension_kind: &'static str,
    /// Routing attributes a trigger's filters match against.
    pub attrs: Vec<(&'static str, String)>,
    /// The host trigger delivered to each matching module.
    pub trigger: Trigger,
}

/// A stream of deliveries the event loop merges and drives.
pub type ExtensionSource = Pin<Box<dyn Stream<Item = ExtensionDelivery> + Send>>;

/// Launch inputs for [`Extension::open_sources`].
pub struct SourceContext<'a> {
    /// The loaded engine config.
    pub config: &'a EngineConfig,
    /// Extension trigger kinds declared by at least one module.
    pub demanded_extension_kinds: &'a BTreeSet<String>,
    executor: &'a TaskExecutor,
    tasks: &'a mut TaskSet,
}

impl<'a> SourceContext<'a> {
    /// Bundle the launch inputs for one [`Extension::open_sources`] pass.
    pub fn new(
        config: &'a EngineConfig,
        demanded_extension_kinds: &'a BTreeSet<String>,
        executor: &'a TaskExecutor,
        tasks: &'a mut TaskSet,
    ) -> Self {
        Self {
            config,
            demanded_extension_kinds,
            executor,
            tasks,
        }
    }

    /// Spawn a source task; it must end when its stream's receiver
    /// drops.
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.push(self.executor.spawn(async move {
            task.await;
            TaskExit::ReceiverGone
        }));
    }
}
