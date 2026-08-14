//! Extension seam: what one extension contributes to the host (namespace,
//! capabilities, linker hook, event sources, and manifest-section install
//! predicates).

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use nexum_tasks::{TaskExecutor, TaskExit, TaskSet};
use wasmtime::component::Linker;
pub use wasmtime_wasi::HostWallClock;

use crate::bindings::nexum::host::types::Event;
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

    /// Subscription kinds this extension's event sources emit; an unknown
    /// non-core kind is refused at boot.
    fn subscriptions(&self) -> &'static [&'static str] {
        &[]
    }

    /// Open the extension's event sources after boot; the event loop merges
    /// and dispatches them.
    fn events(&self, sources: &mut EventSources<'_>) -> anyhow::Result<Vec<ExtensionEventStream>> {
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

/// Event dispatched to every module with a `[[subscription]]` of `kind` whose
/// filters match `attrs`.
pub struct ExtensionEvent {
    /// Manifest subscription kind that routes this event.
    pub kind: &'static str,
    /// Routing attributes a subscription's filters match against.
    pub attrs: Vec<(&'static str, String)>,
    /// The host event delivered to each matching module.
    pub event: Event,
}

/// A stream of extension events the event loop merges and drives.
pub type ExtensionEventStream = Pin<Box<dyn Stream<Item = ExtensionEvent> + Send>>;

/// Launch inputs for [`Extension::events`].
pub struct EventSources<'a> {
    /// The loaded engine config.
    pub config: &'a EngineConfig,
    /// Extension subscription kinds declared by at least one module.
    pub subscribed: &'a BTreeSet<String>,
    executor: &'a TaskExecutor,
    tasks: &'a mut TaskSet,
}

impl<'a> EventSources<'a> {
    /// Bundle the launch inputs for one [`Extension::events`] pass.
    pub fn new(
        config: &'a EngineConfig,
        subscribed: &'a BTreeSet<String>,
        executor: &'a TaskExecutor,
        tasks: &'a mut TaskSet,
    ) -> Self {
        Self {
            config,
            subscribed,
            executor,
            tasks,
        }
    }

    /// Spawn an event-source task; it must end when its stream's receiver
    /// drops.
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.push(self.executor.spawn(async move {
            task.await;
            TaskExit::ReceiverGone
        }));
    }
}
