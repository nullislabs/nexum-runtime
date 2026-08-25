//! Extension seam: what one extension contributes to the host (namespace,
//! capabilities, linker hook, sources, and manifest-section install
//! predicates).

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use nexum_tasks::{TaskExecutor, TaskExit, TaskSet};
use thiserror::Error;
use wasmtime::component::Linker;
pub use wasmtime_wasi::HostWallClock;

use nexum_runtime_config::EngineConfig;
use nexum_runtime_manifest::{ExtensionSections, NamespaceCaps};

use crate::BoxError;
use crate::bindings::nexum::host::types::Trigger;
use crate::runtime_types::RuntimeTypes;

/// A refusal from one [`Extension`] hook.
///
/// Build one with the constructors: `.map_err(|e| ExtensionError::link(NS, e))`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExtensionError {
    /// [`Extension::link`] failed.
    #[error("extension {namespace}: link failed: {source}")]
    Link {
        /// The failing extension's namespace.
        namespace: &'static str,
        /// Why it failed.
        #[source]
        source: BoxError,
    },
    /// [`Extension::admit_worker`] refused a worker.
    #[error("extension refused worker {worker}: {source}")]
    Admit {
        /// The refused worker's namespace.
        worker: String,
        /// Why it refused.
        #[source]
        source: BoxError,
    },
    /// [`Extension::open_sources`] failed.
    #[error("extension {namespace}: open sources failed: {source}")]
    Source {
        /// The failing extension's namespace.
        namespace: &'static str,
        /// Why it failed.
        #[source]
        source: BoxError,
    },
}

impl ExtensionError {
    /// A [`Link`](Self::Link) refusal.
    pub fn link(namespace: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Link {
            namespace,
            source: source.into(),
        }
    }

    /// An [`Admit`](Self::Admit) refusal.
    pub fn admit(worker: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Admit {
            worker: worker.into(),
            source: source.into(),
        }
    }

    /// Named after the hook, since `source` belongs to [`std::error::Error`].
    pub fn open_sources(namespace: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Source {
            namespace,
            source: source.into(),
        }
    }
}

/// One runtime extension; a module importing its interface boots only if both
/// the linker entry and the capability namespace are registered.
pub trait Extension<T: RuntimeTypes>: Send + Sync + 'static {
    /// Namespace this extension owns.
    fn namespace(&self) -> &'static str;

    /// Capability namespace merged into enforcement.
    fn capabilities(&self) -> NamespaceCaps;

    /// Add the extension's imports to a worker linker, after core interfaces
    /// and before instantiation.
    fn link(&self, linker: &mut Linker<T::State>) -> Result<(), ExtensionError>;

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
    fn admit_worker(
        &self,
        worker: &str,
        sections: &ExtensionSections,
    ) -> Result<(), ExtensionError> {
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
    ) -> Result<Vec<ExtensionSource>, ExtensionError> {
        let _ = sources;
        Ok(Vec::new())
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

    /// Spawn a source task under `label`, the name the engine's reports of
    /// it carry; it must end when its stream's receiver drops.
    pub fn spawn(&mut self, label: &str, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.push(
            label,
            self.executor.spawn(async move {
                task.await;
                TaskExit::ReceiverGone
            }),
        );
    }
}
