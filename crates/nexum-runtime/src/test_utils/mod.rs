//! Engine-side mock backends for an in-process runtime on fakes.
//!
//! The chain leg is the real [`ProviderPool`](crate::host::provider_pool::ProviderPool)
//! over in-process mock transports ([`rpc::MockRpc`] scripted,
//! [`rpc::FakeNode`] routed). [`MockStateStore`] is the diskless store seam;
//! [`Prebuilt`] wraps a pre-built instance as a
//! [`ComponentBuilder`](crate::host::component::ComponentBuilder);
//! [`MockTypes`] is the lattice tying them together. Compose through the
//! public builder path:
//!
//! ```no_run
//! # use std::time::Duration;
//! # use nexum_runtime::builder::RuntimeBuilder;
//! # use nexum_runtime::engine_config::EngineConfig;
//! # use nexum_runtime::host::component::ComponentsBuilder;
//! # use nexum_runtime::test_utils::{MockStateStore, MockTypes, Prebuilt};
//! # use nexum_runtime::test_utils::rpc::FakeNode;
//! # async fn demo(config: &EngineConfig) -> anyhow::Result<()> {
//! let node = FakeNode::new();
//! let pool = node.pool(&[alloy_chains::Chain::mainnet()], Duration::from_millis(20));
//! let store = MockStateStore::new();
//! let _handle = RuntimeBuilder::new(config)
//!     .with_types::<MockTypes>()
//!     .with_components(ComponentsBuilder::new(
//!         Prebuilt(pool),
//!         Prebuilt(store.clone()),
//!         (),
//!     ))
//!     .with_add_ons(&[])
//!     .launch()
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod builders;
pub mod clock;
pub mod harness;
pub mod manifest;
pub mod rpc;
pub mod scenario;
mod store;
mod types;
pub mod wasm;

pub use builders::Prebuilt;
pub use harness::{TestRuntime, TestRuntimeBuilder};
pub use manifest::TestManifest;
pub use scenario::{BootScenario, Booted, Refusal};
pub use store::{MockStateHandle, MockStateStore};
pub use types::MockTypes;
pub use wasm::{example_wasm_or_skip, module_wasm, module_wasm_or_skip, test_wasmtime_engine};

use std::collections::HashMap;
use std::time::Duration;

use alloy_chains::Chain;

use crate::engine_config::{ChainConfig, ModuleLimits};
use crate::host::component::Components;
use crate::host::logs::LogPipeline;
use rpc::FakeNode;

/// Poll cadence for harness-driven pollers.
pub(crate) const HARNESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A fresh in-memory [`LogPipeline`] at default retention limits.
pub(crate) fn in_memory_logs() -> LogPipeline {
    LogPipeline::in_memory(ModuleLimits::default().logs())
}

/// `[chains]` entries for every chain id the in-tree test manifests and
/// fixtures name; the `http://` URL is never dialled at boot.
pub fn test_chain_configs() -> HashMap<Chain, ChainConfig> {
    [1, 100, 11_155_111]
        .into_iter()
        .map(|id| {
            (
                Chain::from_id(id),
                ChainConfig {
                    rpc_url: "http://localhost:8545".to_owned(),
                    request_timeout_secs: 30,
                },
            )
        })
        .collect()
}

/// A [`Components`] bundle over fresh mock backends, ready for
/// [`Supervisor::boot`](crate::supervisor::Supervisor::boot).
pub fn mock_components() -> Components<MockTypes> {
    mock_components_from(&FakeNode::new(), MockStateStore::new())
}

/// A [`Components`] bundle serving chain id 1 from `node`, with an empty
/// extension slot and an in-memory log pipeline.
pub fn mock_components_from(node: &FakeNode, store: MockStateStore) -> Components<MockTypes> {
    Components {
        chain: node.pool(&[alloy_chains::Chain::from_id(1)], HARNESS_POLL_INTERVAL),
        store,
        ext: (),
        logs: in_memory_logs(),
    }
}

#[cfg(test)]
mod tests {
    use super::rpc::FakeNode;
    use super::*;
    use alloy_chains::Chain;
    use futures::StreamExt as _;

    use crate::builder::RuntimeBuilder;
    use crate::engine_config::EngineConfig;
    use crate::host::component::{ChainMethod, ComponentsBuilder, StateHandle, StateStore};

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
                (),
            ))
            .with_add_ons(&[])
            .launch()
            .await
        {
            Ok(_) => panic!("default config declares no modules; launch must bail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("no modules to run"), "{err}");

        // The fake actually serves and records, independent of the launch.
        let body = pool
            .request(Chain::from_id(1), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .expect("canned response");
        assert_eq!(body, "\"0x10\"");
        assert_eq!(node.recorded_requests().len(), 1);
    }

    #[tokio::test]
    async fn pool_rejects_an_unconfigured_chain_before_the_node() {
        use crate::host::provider_pool::PoolError;

        let node = FakeNode::new();
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        let err = pool
            .request(Chain::from_id(2), ChainMethod::EthBlockNumber, "[]".into())
            .await
            .expect_err("chain 2 is not registered");
        assert!(matches!(err, PoolError::UnknownChain(c) if c == Chain::from_id(2)));
        assert!(
            node.recorded_requests().is_empty(),
            "the lookup failure never reaches the transport",
        );
    }

    #[tokio::test]
    async fn subscribe_blocks_yields_pushed_headers() {
        let node = FakeNode::new();
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        let mut header: alloy_rpc_types_eth::Header = alloy_rpc_types_eth::Header::default();
        header.inner.number = 7;
        node.push_block(header);
        let mut stream = pool
            .subscribe_blocks(Chain::from_id(1))
            .await
            .expect("block stream");
        let item = stream
            .next()
            .await
            .expect("one item")
            .expect("pushed header arrives as Ok");
        assert_eq!(item.number, 7);
    }

    #[tokio::test]
    async fn watch_chain_logs_yields_pushed_logs() {
        let node = FakeNode::new();
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        node.push_chain_log(alloy_rpc_types_eth::Log::default());
        let mut stream = pool
            .watch_chain_logs(Chain::from_id(1), Default::default(), 1)
            .expect("chain-log poller stream");
        let batch = stream
            .next()
            .await
            .expect("one item")
            .expect("pushed log arrives as an Ok batch");
        assert_eq!(batch.number, 1, "a default log lands one past the head");
        assert_eq!(batch.logs.len(), 1);
        assert!(!batch.removed);
    }

    /// The store round-trips values, isolates namespaces, lists by prefix,
    /// and rejects the empty namespace.
    #[test]
    fn store_roundtrips_and_isolates_namespaces() {
        let store = MockStateStore::new();
        let a = store.module("mod-a").expect("namespace a");
        let b = store.module("mod-b").expect("namespace b");

        a.set("k", b"va").expect("set a");
        b.set("k", b"vb").expect("set b");
        assert_eq!(a.get("k").expect("get a").as_deref(), Some(&b"va"[..]));
        assert_eq!(b.get("k").expect("get b").as_deref(), Some(&b"vb"[..]));

        a.set("k2", b"x").expect("set a k2");
        assert_eq!(
            a.list_keys("k").expect("list a"),
            vec!["k".to_owned(), "k2".to_owned()],
        );

        a.delete("k").expect("delete a k");
        assert!(a.get("k").expect("get a k").is_none());

        assert!(store.module("").is_err(), "empty namespace rejected");
    }
}
