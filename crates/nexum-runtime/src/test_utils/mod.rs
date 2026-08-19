//! Engine-side mock backends for an in-process runtime on fakes.
//!
//! The chain leg is the real [`ProviderPool`](crate::component::ProviderPool)
//! over in-process mock transports ([`MockRpc`] scripted,
//! [`FakeNode`] routed). [`MockStateStore`] is the diskless store seam;
//! [`Prebuilt`] wraps a pre-built instance as a
//! [`ComponentBuilder`](crate::component::ComponentBuilder);
//! [`MockTypes`] is the lattice tying them together. Compose through the
//! public builder path:
//!
//! ```no_run
//! # use std::time::Duration;
//! # use nexum_runtime::RuntimeBuilder;
//! # use nexum_runtime::component::ComponentsBuilder;
//! # use nexum_runtime::config::EngineConfig;
//! # use nexum_runtime::test_utils::{FakeNode, MockStateStore, MockTypes, Prebuilt};
//! # async fn demo(config: &EngineConfig) -> Result<(), nexum_runtime::error::RuntimeError> {
//! let node = FakeNode::new();
//! let pool = node.pool(&[alloy_chains::Chain::mainnet()], Duration::from_millis(20));
//! let store = MockStateStore::new();
//! let _handle = RuntimeBuilder::new(config)
//!     .with_types::<MockTypes>()
//!     .with_components(ComponentsBuilder::new(Prebuilt(pool), Prebuilt(store.clone())))
//!     .launch()
//!     .await?;
//! # Ok(())
//! # }
//! ```

// Test support, gated behind `feature = "test-utils"` rather than
// `cfg(test)`, so `allow-unwrap-in-tests` never sees it. A harness that
// cannot build its own fixture has nothing to recover from.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod harness;

pub use harness::{TestRuntime, TestRuntimeBuilder};
pub use nexum_runtime_supervisor::test_utils::{
    BootScenario, Booted, Entry, Refusal, test_wasmtime_engine,
};
pub use nexum_runtime_testing::{
    ALLOW_MISSING_WASM, CapturedRpc, FakeNode, JsonValue, ManifestInput, ManualClock, MockResponse,
    MockRpc, MockStateHandle, MockStateStore, MockTypes, Prebuilt, Sample, Serialize, TestManifest,
    alloy_json_rpc, capture_metrics, example_wasm_or_skip, linked_block, manifest, metrics_util,
    mock_components, mock_components_from, mocked_pool, module_wasm, module_wasm_or_skip, rpc_err,
    rpc_head, rpc_ok, samples_named, target_dir, test_chain_configs, test_hash, tower,
    workspace_root,
};

pub(crate) use nexum_runtime_testing::{HARNESS_POLL_INTERVAL, in_memory_logs};

/// A [`BootScenario`] over [`CoreRuntime`](crate::CoreRuntime).
pub fn core_scenario() -> BootScenario<crate::CoreRuntime> {
    BootScenario::new()
}

#[cfg(test)]
pub(crate) fn limits_with(
    set: impl FnOnce(&mut crate::engine_config::ModuleLimits),
) -> crate::engine_config::ModuleLimits {
    let mut limits = crate::engine_config::ModuleLimits::default();
    set(&mut limits);
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_chains::Chain;

    use crate::builder::RuntimeBuilder;
    use crate::engine_config::EngineConfig;
    use crate::error::LaunchRefusal;
    use nexum_runtime_wasm::ComponentsBuilder;
    use nexum_world::ChainMethod;

    /// A custom component set launches through the public builder on fakes;
    /// it bails at boot only because the default config declares no modules,
    /// proving the mock backends composed and the build path ran.
    #[tokio::test]
    async fn m0_custom_component_set_launches_through_the_public_builder() {
        let node = FakeNode::new();
        node.on_method(ChainMethod::EthBlockNumber, "\"0x10\"");
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        let store = MockStateStore::new();

        let config = EngineConfig::default();
        let err = match RuntimeBuilder::new(&config)
            .with_types::<MockTypes>()
            .with_components(ComponentsBuilder::new(
                Prebuilt(pool.clone()),
                Prebuilt(store),
            ))
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        Refusal::from(err).variant::<LaunchRefusal>(|e| matches!(e, LaunchRefusal::NothingToRun));

        // The fake actually serves and records, independent of the launch.
        let body = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .expect("canned response");
        assert_eq!(body, "\"0x10\"");
        assert_eq!(node.recorded_requests().len(), 1);
    }
}
