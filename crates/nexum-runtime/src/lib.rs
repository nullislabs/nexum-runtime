//! Wasmtime-based host for WASM Component Model modules, embeddable as a
//! library; the bundled binary is a thin consumer of the same surface.
//!
//! Settlement-domain-agnostic: no domain symbol or WIT reference, `nexum:host`
//! stays a leaf WIT package, no crate edge reaches a domain crate. Enforced in
//! CI by `scripts/zero-leak.sh`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![forbid(unsafe_code)]

pub use {
    alloy_chains, alloy_primitives, alloy_provider, alloy_rpc_types_eth, alloy_transport, futures,
    nexum_tasks, nexum_world, toml, wasmtime, wasmtime_wasi, wasmtime_wasi_http,
};

/// Markers reserved for semver evolution of [`Runtime`] and
/// [`component::RuntimeTypes`]: implement alongside the trait.
#[doc(hidden)]
pub mod sealed {
    pub use nexum_runtime_api::sealed::SealedRuntimeTypes;

    pub trait SealedRuntime {}
}

pub use nexum_runtime_api::bindings;

pub mod addons;
mod builder;
mod preset;

pub use nexum_runtime_supervisor::error;

/// Multi-module supervisor: loads `engine.toml` entries, one wasmtime `Store`
/// each, and routes triggers.
pub mod supervisor {
    pub use nexum_runtime_supervisor::supervisor::{
        BootEnv, ConfiguredChains, EventSource, SourcePlan, Supervisor, Viability,
        WasiClockOverride, build_linker,
    };
}

pub(crate) use nexum_runtime_config as engine_config;

/// `component.toml` parser and capability enforcement.
pub mod manifest {
    pub use nexum_primitives::interface_id::{InterfaceId, InterfaceTrack};
    pub use nexum_runtime_manifest::{CapabilityRegistry, ExtensionSections, NamespaceCaps};
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
        LogBoundsPolicy, LogFilterPolicy, LogLimitsSection, LogRetentionLimits, LogVerdict,
        MetricsSection, ModuleEntry, ModuleLimits, OutboundHttpLimits, PoisonLimitsSection,
        PoisonPolicy, PolicyCeilings, PolicySection, ResolvedModuleLimits, RpcEndpoint,
        RpcTransport, ShutdownLimitsSection, TotalPolicy, load_or_default,
    };
    pub use url::Url;
}

/// Backend component seams and the builders that open them.
pub mod component {
    pub use nexum_runtime_api::{
        BuilderContext, ComponentBuilder, Handle, MAX_APPLY_OPS, MAX_APPLY_VALUE_BYTES,
        RuntimeTypes, StateHandle, StateStore, StoreError, WriteOp,
    };
    pub use nexum_runtime_chain::{
        BlockStream, CanonicalLogBatch, CanonicalLogStream, PoolError, ProviderPool,
        ProviderPoolBuilder,
    };
    pub use nexum_runtime_logs::LogPipelineBuilder;
    pub use nexum_runtime_store::{LocalStore, LocalStoreBuilder, ModuleStore, StorageError};
    pub use nexum_runtime_wasm::{BuildError, Components, ComponentsBuilder};
    pub use nexum_world::ChainMethod;
    pub use redb::{
        CommitError, DatabaseError, StorageError as RedbStorageError, TableError, TransactionError,
    };
}

/// The seam an extension author implements against.
pub mod extension {
    pub use nexum_runtime_api::{
        Extension, ExtensionDelivery, ExtensionError, ExtensionSource, HostWallClock, SourceContext,
    };
    pub use nexum_runtime_http::HttpGate;
    pub use nexum_runtime_wasm::{HostState, fault_label, fault_message};
}

/// The module-log pipeline and its read surface.
pub mod logs {
    pub use nexum_runtime_logs::{
        InMemoryRunLogStore, LogChannel, LogField, LogPage, LogPipeline, LogRecord, LogRouter,
        LogSource, LogValue, RunId, RunLogStore, RunMeta, StdioStream,
    };
    pub use tokio::sync::Notify;
    pub use tracing_core::Level;
}

/// The dispatch loop, for an embedder driving a [`supervisor::Supervisor`] directly.
pub mod event_loop {
    pub use nexum_runtime_supervisor::event_loop::{
        ChainLogItem, RunEnd, RunOutcome, TaggedBlockStream, TaggedChainLog, TaggedChainLogStream,
        open_block_streams, open_chain_log_streams, run, wait_for_os_signal,
    };
    pub use nexum_tasks::SourceTermination;
}

pub use builder::{
    AssembledRuntime, LaunchContext, PresetBuilder, PresetComponentsBuilder, ReadyBuilder,
    RuntimeBuilder, RuntimeHandle, TypedBuilder,
};
pub use nexum_primitives::module_id::ModuleId;
pub use preset::{CoreRuntime, Runtime};
