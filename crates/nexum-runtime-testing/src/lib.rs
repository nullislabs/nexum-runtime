//! Seam-level test doubles for the Nexum runtime.

#![forbid(unsafe_code)]
// Test support, so `allow-unwrap-in-tests` never sees it. A harness that
// cannot build its own fixture has nothing to recover from.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod builders;
mod clock;
mod json_logs;
mod manifest;
mod metrics_capture;
mod rpc;
mod store;
mod types;
mod wasm;

pub use serde::Serialize;
pub use serde_json::Value as JsonValue;
pub use {alloy_json_rpc, metrics_util, tower};

pub use alloy_transport::mock::MockResponse;
pub use builders::Prebuilt;
pub use clock::ManualClock;
pub use json_logs::{JsonLogs, json_collector};
pub use manifest::{ManifestInput, TestManifest, manifest};
pub use metrics_capture::{Sample, capture_metrics, samples_named};
pub use rpc::{
    CapturedRpc, FakeNode, MockRpc, linked_block, mocked_pool, rpc_err, rpc_head, rpc_ok, test_hash,
};
pub use store::{MockStateHandle, MockStateStore};
pub use types::MockTypes;
pub use wasm::{
    ALLOW_MISSING_WASM, example_wasm_or_skip, module_wasm, module_wasm_or_skip, target_dir,
    workspace_root,
};

use std::collections::HashMap;
use std::time::Duration;

use alloy_chains::Chain;

use nexum_runtime_config::{ChainConfig, ResolvedModuleLimits};
use nexum_runtime_logs::LogPipeline;
use nexum_runtime_wasm::Components;

#[doc(hidden)]
pub const HARNESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A fresh in-memory [`LogPipeline`] at default retention limits.
#[doc(hidden)]
pub fn in_memory_logs() -> LogPipeline {
    LogPipeline::in_memory(ResolvedModuleLimits::default().logs)
}

/// `[chains]` entries for every chain id the test fixtures name; never dialled at boot.
pub fn test_chain_configs() -> HashMap<Chain, ChainConfig> {
    let rpc_url = match nexum_runtime_config::RpcEndpoint::try_from("http://localhost:8545") {
        Ok(endpoint) => endpoint,
        Err(_) => unreachable!("the literal test URL parses"),
    };
    [1, 100, 11_155_111]
        .into_iter()
        .map(|id| {
            (
                Chain::from_id(id),
                ChainConfig {
                    rpc_url: rpc_url.clone(),
                    request_timeout_secs: 30,
                    max_log_range_blocks: 1000,
                },
            )
        })
        .collect()
}

/// A [`Components`] bundle over fresh mock backends.
pub fn mock_components() -> Components<MockTypes> {
    mock_components_from(&FakeNode::new(), MockStateStore::new())
}

/// A [`Components`] bundle serving chain id 1 from `node`, with an in-memory
/// log pipeline.
pub fn mock_components_from(node: &FakeNode, store: MockStateStore) -> Components<MockTypes> {
    Components {
        chain: node.pool(&[alloy_chains::Chain::from_id(1)], HARNESS_POLL_INTERVAL),
        store,
        logs: in_memory_logs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_chains::Chain;
    use futures::StreamExt as _;

    use nexum_runtime_api::{StateHandle, StateStore};
    use nexum_world::ChainMethod;

    #[tokio::test]
    async fn pool_rejects_an_unconfigured_chain_before_the_node() {
        use nexum_runtime_chain::PoolError;

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
    async fn open_block_source_yields_pushed_headers() {
        let node = FakeNode::new();
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        let mut header: alloy_rpc_types_eth::Header = alloy_rpc_types_eth::Header::default();
        header.inner.number = 7;
        node.push_block(header);
        let mut stream = pool
            .open_block_source(Chain::from_id(1))
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
    async fn open_event_source_yields_pushed_logs() {
        let node = FakeNode::new();
        let pool = node.pool(&[Chain::from_id(1)], HARNESS_POLL_INTERVAL);
        node.push_chain_log(alloy_rpc_types_eth::Log::default());
        let mut stream = pool
            .open_event_source(Chain::from_id(1), Default::default(), 1)
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

    /// The fake pages, filters and resumes as the redb backend does, so a
    /// harness test of a scan is not testing a different scan.
    #[test]
    fn store_pages_filtered_entries_and_resumes_from_the_last_examined() {
        use nexum_runtime_api::{ListQuery, ValueFilter};

        let store = MockStateStore::new();
        let a = store.module("mod-a").expect("namespace a");
        a.set("p:1", b"\x01one").expect("set 1");
        a.set("p:2", b"\x02two").expect("set 2");
        a.set("p:3", b"\x01three").expect("set 3");
        let page = |start_after, scan_limit, filter| {
            a.list_entries(&ListQuery {
                prefix: "p:",
                start_after,
                limit: 0,
                scan_limit,
                filter,
            })
            .expect("page")
        };

        let empty = page("", 2, Some(ValueFilter::HasPrefix(&[0x09])));
        assert!(empty.entries.is_empty());
        assert!(!empty.exhausted);
        assert_eq!(empty.last_examined.as_deref(), Some("p:2"));

        let resumed = page("p:2", 0, Some(ValueFilter::LacksPrefix(&[0x02])));
        assert_eq!(keys(&resumed), ["p:3"]);
        assert!(resumed.exhausted);
    }

    fn keys(page: &nexum_runtime_api::EntryPage) -> Vec<&str> {
        page.entries.iter().map(|(k, _)| k.as_str()).collect()
    }
}
