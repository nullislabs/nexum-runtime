//! Wasmtime-based host for WASM Component Model modules, embeddable as a
//! library; the bundled binary is a thin consumer of the same surface.
//!
//! Settlement-domain-agnostic: no domain symbol or WIT reference, `nexum:host`
//! stays a leaf WIT package, no crate edge reaches a domain crate. Enforced in
//! CI by `scripts/zero-leak.sh`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![forbid(unsafe_code)]

// alloy split its API across multiple crates; we depend on the
// transports directly so cargo resolves the right feature set, but
// the runtime code only names them through the `alloy_provider`
// re-exports. Silence `unused_crate_dependencies` with `as _`.
use alloy_rpc_client as _;
use alloy_transport_ws as _;

pub use {
    alloy_chains, alloy_primitives, alloy_provider, alloy_rpc_types_eth, alloy_transport, futures,
    nexum_tasks, nexum_world, toml, wasmtime, wasmtime_wasi, wasmtime_wasi_http,
};

/// Markers reserved for semver evolution of [`Runtime`] and
/// [`component::RuntimeTypes`]: implement alongside the trait.
#[doc(hidden)]
pub mod sealed {
    pub trait SealedRuntimeTypes {}
    pub trait SealedRuntime {}
}

pub mod addons;
pub mod bindings;
mod builder;
pub mod error;
mod host;
#[path = "metrics.rs"]
mod metric_names;
mod preset;
mod runtime;
pub mod supervisor;

pub(crate) use nexum_runtime_config as engine_config;

/// `component.toml` parser and capability enforcement.
pub mod manifest {
    pub use nexum_primitives::interface_id::{InterfaceId, InterfaceTrack};
    pub(crate) use nexum_runtime_manifest::error;
    pub use nexum_runtime_manifest::{CapabilityRegistry, ExtensionSections, NamespaceCaps};
    pub(crate) use nexum_runtime_manifest::{
        LoadedManifest, ParseError, ResourceSection, Trigger, enforce_capabilities, load,
    };
}

#[cfg(feature = "test-utils")]
pub mod test_utils;

/// Engine-side configuration (`engine.toml`) and its resolved forms.
pub mod config {
    pub use ipnet::IpNet;
    pub use nexum_primitives::digest::{ContentDigest, DigestPin};
    pub use nexum_primitives::host_pattern::HostPattern;
    pub use nexum_runtime_config::{
        ChainConfig, ChainLimitsSection, ComponentPolicy, DispatchLimitsSection,
        DispatchRatePolicy, EffectivePolicy, EngineConfig, EngineSection, HttpLimitsSection,
        LogLimitsSection, LogRetentionLimits, MetricsSection, ModuleEntry, ModuleLimits,
        OutboundHttpLimits, PoisonLimitsSection, PoisonPolicy, PolicyCeilings, PolicySection,
        ResolvedModuleLimits, RpcEndpoint, RpcTransport, ShutdownLimitsSection, TotalPolicy,
        load_or_default,
    };
    pub use url::Url;
}

/// Backend component seams and the builders that open them.
pub mod component {
    pub use crate::host::component::{
        BuildError, BuilderContext, ChainMethod, ComponentBuilder, Components, ComponentsBuilder,
        Handle, LocalStoreBuilder, LogPipelineBuilder, ProviderPoolBuilder, RuntimeTypes,
        StateHandle, StateStore,
    };
    pub use crate::host::local_store_redb::{
        LocalStore, MAX_APPLY_OPS, MAX_APPLY_VALUE_BYTES, ModuleStore, StorageError, WriteOp,
    };
    pub use crate::host::provider_pool::{
        BlockStream, CanonicalLogBatch, CanonicalLogStream, PoolError, ProviderPool,
    };
    pub use redb::{
        CommitError, DatabaseError, StorageError as RedbStorageError, TableError, TransactionError,
    };
}

/// The seam an extension author implements against.
pub mod extension {
    pub use crate::host::extension::{
        Extension, ExtensionDelivery, ExtensionError, ExtensionSource, HostWallClock, SourceContext,
    };
    pub use crate::host::fault::{fault_label, fault_message};
    pub use crate::host::http::HttpGate;
    pub use crate::host::state::HostState;
}

/// The module-log pipeline and its read surface.
pub mod logs {
    pub use crate::host::logs::{
        InMemoryRunLogStore, LogChannel, LogPage, LogPipeline, LogRecord, LogRouter, RunId,
        RunLogStore, RunMeta, StdioStream,
    };
    pub use tokio::sync::Notify;
    pub use tracing_core::Level;
}

/// The dispatch loop, for an embedder driving a [`supervisor::Supervisor`] directly.
pub mod event_loop {
    pub use crate::runtime::event_loop::{
        TaggedBlockStream, TaggedChainLog, TaggedChainLogStream, open_block_streams,
        open_chain_log_streams, run, wait_for_os_signal,
    };
}

pub use builder::{
    AssembledRuntime, LaunchContext, PresetBuilder, PresetComponentsBuilder, ReadyBuilder,
    RuntimeBuilder, RuntimeHandle, TypedBuilder,
};
pub use nexum_primitives::module_id::ModuleId;
pub use preset::{CoreRuntime, Runtime};
