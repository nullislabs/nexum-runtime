//! Boot-path test helpers over the seam-level doubles in
//! `nexum-runtime-testing`.

// Test support, gated behind `feature = "test-utils"` rather than
// `cfg(test)`, so `allow-unwrap-in-tests` never sees it. A harness that
// cannot build its own fixture has nothing to recover from.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod scenario;

pub use scenario::{BootScenario, Booted, Entry, Refusal};

pub(crate) use nexum_runtime_testing::{
    ManifestInput, TestManifest, in_memory_logs, test_chain_configs,
};

#[cfg(test)]
pub(crate) use nexum_runtime_testing::{
    FakeNode, ManualClock, MockRpc, MockStateStore, MockTypes, capture_metrics, crate_source_roots,
    example_wasm_or_skip, linked_block, metrics_util, mock_components, mock_components_from,
    mocked_pool, module_wasm_or_skip, rpc_err, rpc_head, rpc_ok, samples_named, test_hash,
    workspace_root,
};

#[cfg(test)]
pub(crate) struct LocalTypes;

#[cfg(test)]
impl nexum_runtime_api::sealed::SealedRuntimeTypes for LocalTypes {}

#[cfg(test)]
impl nexum_runtime_api::RuntimeTypes for LocalTypes {
    type State = nexum_runtime_wasm::HostState<Self>;
    type Store = nexum_runtime_store::LocalStore;
}

/// Test engine built from the production launch config.
pub fn test_wasmtime_engine() -> wasmtime::Engine {
    wasmtime::Engine::new(&crate::supervisor::wasmtime_config()).expect("wasmtime engine")
}

#[cfg(test)]
pub(crate) fn limits_with(
    set: impl FnOnce(&mut crate::engine_config::ModuleLimits),
) -> crate::engine_config::ModuleLimits {
    let mut limits = crate::engine_config::ModuleLimits::default();
    set(&mut limits);
    limits
}
