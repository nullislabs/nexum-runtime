//! Event trigger filters, log projection, cursor keys and persistence.

use super::*;

#[test]
fn alloy_filter_with_address_and_topic() {
    let addr = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let topic = "0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
    let filter = build_alloy_filter(Some(addr.parse().unwrap()), Some(topic.parse().unwrap()));
    // alloy `Filter` exposes no getter; assert through its serialization.
    let serialized = serde_json::to_value(&filter).unwrap();
    let addr_field = serialized
        .get("address")
        .unwrap()
        .to_string()
        .to_lowercase();
    assert!(addr_field.contains(&addr.to_lowercase()[2..])); // strip 0x
}

#[test]
fn alloy_filter_no_address_no_topic() {
    let filter = build_alloy_filter(None, None);
    let serialized = serde_json::to_value(&filter).unwrap();
    assert!(
        serialized.get("address").is_none()
            || serialized["address"].is_null()
            || serialized["address"] == serde_json::json!([])
    );
}

/// A mined log carries every block-scoped field; the host projection must
/// preserve each one so the guest rebuilds the native alloy log losslessly.
#[test]
fn project_log_preserves_mined_log() {
    use alloy_primitives::{Address, B256, Bytes};

    let address = Address::repeat_byte(0x11);
    let topics = vec![B256::repeat_byte(0x22), B256::repeat_byte(0x33)];
    let data = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
    let inner = alloy_primitives::Log::new_unchecked(address, topics.clone(), data.clone());

    let log = alloy_rpc_types_eth::Log {
        inner,
        block_hash: Some(B256::repeat_byte(0x44)),
        block_number: Some(0x1234),
        block_timestamp: Some(0x5678),
        transaction_hash: Some(B256::repeat_byte(0x55)),
        transaction_index: Some(7),
        log_index: Some(9),
        removed: true,
    };

    let projected = wit_log(&log, Chain::from_id(11_155_111));

    assert_eq!(projected.chain_id, 11_155_111);
    assert_eq!(projected.address, address.as_slice().to_vec());
    assert_eq!(
        projected.topics,
        topics
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect::<Vec<_>>(),
    );
    assert_eq!(projected.data, data.to_vec());
    assert_eq!(
        projected.block_hash.as_deref(),
        Some(B256::repeat_byte(0x44).as_slice()),
    );
    assert_eq!(projected.block_number, Some(0x1234));
    assert_eq!(projected.block_timestamp, Some(0x5678));
    assert_eq!(
        projected.transaction_hash.as_deref(),
        Some(B256::repeat_byte(0x55).as_slice()),
    );
    assert_eq!(projected.transaction_index, Some(7));
    assert_eq!(projected.log_index, Some(9));
    assert!(projected.removed);
}

/// A pending log has no block-scoped fields; the projection must leave each
/// one `None` rather than collapsing an absent value onto a zero default.
#[test]
fn project_log_leaves_pending_fields_none() {
    use alloy_primitives::{Address, Bytes};

    let inner =
        alloy_primitives::Log::new_unchecked(Address::repeat_byte(0xab), Vec::new(), Bytes::new());
    let log = alloy_rpc_types_eth::Log {
        inner,
        block_hash: None,
        block_number: None,
        block_timestamp: None,
        transaction_hash: None,
        transaction_index: None,
        log_index: None,
        removed: false,
    };

    let projected = wit_log(&log, Chain::from_id(1));

    assert!(projected.block_hash.is_none());
    assert!(projected.block_number.is_none());
    assert!(projected.block_timestamp.is_none());
    assert!(projected.transaction_hash.is_none());
    assert!(projected.transaction_index.is_none());
    assert!(projected.log_index.is_none());
    assert!(projected.topics.is_empty());
    assert!(projected.data.is_empty());
    assert!(!projected.removed);
}

/// Data-compat guard: the typed derivation must reproduce the key formerly
/// keccak'd from the lowercased `0x`-prefixed manifest strings, so a resume
/// cursor written before values were typed still seeds the same stream.
#[test]
fn chainlog_cursor_key_matches_the_legacy_string_derivation() {
    let addr = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let topic = "0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
    let key = chainlog_cursor_key(
        Chain::from_id(1),
        Some(addr.parse().unwrap()),
        Some(topic.parse().unwrap()),
    );
    let legacy = format!("1|{}|{}", addr.to_ascii_lowercase(), topic);
    assert_eq!(
        key,
        format!(
            "chainlog_cursor:{:x}",
            alloy_primitives::keccak256(legacy.as_bytes())
        ),
    );
}

#[test]
fn chainlog_cursor_key_differs_by_each_input() {
    use alloy_primitives::{Address, B256};

    let addr = Address::repeat_byte(0xab);
    let topic = B256::repeat_byte(0xde);
    let base = chainlog_cursor_key(Chain::from_id(1), Some(addr), Some(topic));
    assert!(
        base.starts_with("chainlog_cursor:"),
        "key carries the prefix: {base}"
    );
    assert_ne!(
        base,
        chainlog_cursor_key(Chain::from_id(10), Some(addr), Some(topic)),
        "chain id is part of the key",
    );
    assert_ne!(
        base,
        chainlog_cursor_key(
            Chain::from_id(1),
            Some(Address::repeat_byte(0x99)),
            Some(topic)
        ),
        "address is part of the key",
    );
    assert_ne!(
        base,
        chainlog_cursor_key(Chain::from_id(1), Some(addr), None),
        "topic presence changes the key",
    );
    assert_ne!(
        base,
        chainlog_cursor_key(Chain::from_id(1), None, Some(topic)),
        "address presence changes the key",
    );
}

#[test]
fn cursor_record_only_moves_an_addition_forward() {
    let mut cursors = ChainLogCursors::default();
    assert_eq!(cursors.record("mod", "key", 100, false, || None), Some(100));
    assert_eq!(
        cursors.record("mod", "key", 90, false, || None),
        None,
        "a replayed height is a no-op"
    );
    assert_eq!(
        cursors.record("mod", "key", 100, false, || None),
        None,
        "an equal height is a no-op"
    );
    assert_eq!(cursors.record("mod", "key", 101, false, || None), Some(101));
}

#[test]
fn cursor_record_rewinds_on_a_retraction() {
    let mut cursors = ChainLogCursors::default();
    assert_eq!(cursors.record("mod", "key", 100, false, || None), Some(100));
    assert_eq!(
        cursors.record("mod", "key", 90, true, || None),
        Some(90),
        "a retraction rewinds to the retracted height"
    );
    assert_eq!(
        cursors.record("mod", "key", 150, true, || None),
        None,
        "a retraction above the cursor never advances it"
    );
}

#[test]
fn cursor_record_seeds_from_the_persisted_cursor_once() {
    let mut cursors = ChainLogCursors::default();
    assert_eq!(
        cursors.record("mod", "key", 100, false, || Some(100)),
        None,
        "the boot replay of the persisted cursor block is a no-op",
    );
    assert_eq!(
        cursors.record("mod", "key", 101, false, || Some(500)),
        Some(101),
        "the seed is consulted only on first sight, never re-applied",
    );
}

#[test]
fn cursor_record_unseeded_writes_the_first_block() {
    let mut cursors = ChainLogCursors::default();
    assert_eq!(cursors.record("mod", "key", 0, false, || None), Some(0));
    assert_eq!(cursors.record("mod", "key", 0, false, || None), None);
}

#[test]
fn cursor_record_is_per_module() {
    let mut cursors = ChainLogCursors::default();
    assert_eq!(
        cursors.record("mod-a", "key", 100, false, || None),
        Some(100)
    );
    assert_eq!(
        cursors.record("mod-b", "key", 50, false, || None),
        Some(50),
        "other module unaffected"
    );
    assert_eq!(
        cursors.record("mod-a", "key2", 50, false, || None),
        Some(50),
        "other key unaffected"
    );
}

#[test]
fn commit_chain_log_cursor_persists_the_monotonic_max() {
    let (_dir, store) = temp_local_store();
    let mut cursors = ChainLogCursors::default();
    let commit = |cursors: &mut ChainLogCursors, block, removed| {
        commit_chain_log_cursor(&store, cursors, "mod", "key", block, removed);
    };

    commit(&mut cursors, 100, false);
    assert_eq!(read_chain_log_cursor(&store, "mod", "key"), Some(100));
    commit(&mut cursors, 90, false);
    assert_eq!(
        read_chain_log_cursor(&store, "mod", "key"),
        Some(100),
        "a replayed height never rewinds the persisted cursor",
    );

    // A fresh mirror models an engine restart.
    let mut restarted = ChainLogCursors::default();
    commit(&mut restarted, 50, false);
    assert_eq!(
        read_chain_log_cursor(&store, "mod", "key"),
        Some(100),
        "the persisted cursor seeds the mirror, so a replay after a restart holds",
    );
    commit(&mut restarted, 101, false);
    assert_eq!(read_chain_log_cursor(&store, "mod", "key"), Some(101));

    commit(&mut restarted, 95, true);
    assert_eq!(
        read_chain_log_cursor(&store, "mod", "key"),
        Some(95),
        "a retraction rewinds the persisted cursor to the retracted height",
    );
}

#[test]
fn frontier_commits_persist_until_the_pair_is_held() {
    let (_dir, store) = temp_local_store();
    let mut cursors = ChainLogCursors::default();
    commit_chain_log_frontier(&store, &mut cursors, "mod", "key", 800);
    assert_eq!(read_chain_log_cursor(&store, "mod", "key"), Some(800));

    cursors.hold("mod", "key");
    commit_chain_log_frontier(&store, &mut cursors, "mod", "key", 1_600);
    assert_eq!(
        read_chain_log_cursor(&store, "mod", "key"),
        Some(800),
        "a held pair keeps the cursor where the failure left it",
    );
    commit_chain_log_cursor(&store, &mut cursors, "mod", "key", 1_700, false);
    assert_eq!(
        read_chain_log_cursor(&store, "mod", "key"),
        Some(1_700),
        "a successful dispatch still commits under a hold",
    );
}

#[tokio::test]
async fn a_failed_event_dispatch_withholds_later_frontier_commits() {
    let mut booted = BootScenario::over(mock_components())
        .boot()
        .await
        .expect("boot mock supervisor");
    let module = nexum_primitives::module_id::ModuleId::parse("ghost").expect("valid module name");

    booted
        .supervisor
        .commit_chain_log_frontier(&module, "key", 800);
    let store = booted.supervisor.shared.components.store.clone();
    assert_eq!(read_chain_log_cursor(&store, "ghost", "key"), Some(800));

    // No module named "ghost" is loaded, so the dispatch fails.
    let ok = booted
        .supervisor
        .dispatch_event(
            &module,
            Chain::mainnet(),
            alloy_rpc_types_eth::Log::default(),
            Some("key"),
        )
        .await;
    assert!(!ok);

    booted
        .supervisor
        .commit_chain_log_frontier(&module, "key", 1_600);
    assert_eq!(
        read_chain_log_cursor(&store, "ghost", "key"),
        Some(800),
        "the frontier no longer advances past the failed dispatch",
    );
}

#[test]
fn the_persisted_cursor_is_invisible_to_the_module_namespace() {
    let (_dir, store) = temp_local_store();
    let mut cursors = ChainLogCursors::default();
    commit_chain_log_cursor(&store, &mut cursors, "mod", "key", 100, false);
    assert_eq!(read_chain_log_cursor(&store, "mod", "key"), Some(100));

    let module_handle = store.module("mod").unwrap();
    assert_eq!(module_handle.get("key").unwrap(), None);
    assert_eq!(module_handle.list_keys("").unwrap(), Vec::<String>::new());
}

#[test]
fn host_cursor_bytes_never_charge_the_author_quota() {
    let (_dir, store) = temp_local_store();
    let module_handle = store.module("mod").unwrap().with_quota(300);
    module_handle
        .set("a", &[0u8; 100])
        .expect("the author's first write fits the quota");

    let mut cursors = ChainLogCursors::default();
    for block in [100, 101, 102] {
        commit_chain_log_cursor(&store, &mut cursors, "mod", "key", block, false);
    }
    assert_eq!(read_chain_log_cursor(&store, "mod", "key"), Some(102));

    module_handle
        .set("b", &[0u8; 3])
        .expect("the cursor commits leave the author's quota headroom intact");
}

/// `start_block` seeds the first boot only. A module whose whole state
/// derives from logs cannot start at head, because history it never saw is
/// history it can never rebuild: a conditional order created before the
/// daemon first ran would otherwise never be polled. Once a cursor exists
/// the store wins, so the seed is a one-time floor and not a rescan point.
#[tokio::test]
async fn start_block_seeds_the_first_boot_and_then_yields_to_the_stored_cursor() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    const SEED: u64 = 17_883_049;
    const ADVANCED: u64 = SEED + 5_000;

    let booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .event_trigger_resuming(1, Some(SEED)),
        )
        .boot()
        .await
        .expect("the example boots alive");

    let sources = booted.supervisor.source_plan().event_sources;
    assert_eq!(sources.len(), 1);
    let key = sources[0]
        .cursor_key
        .clone()
        .expect("a resuming trigger carries a cursor key");
    assert_eq!(
        sources[0].initial_cursor,
        Some(SEED),
        "an empty store falls back to the declared start block",
    );

    let store = booted.supervisor.shared.components.store.clone();
    let mut cursors = ChainLogCursors::default();
    commit_chain_log_cursor(&store, &mut cursors, "example", &key, ADVANCED, false);

    assert_eq!(
        booted.supervisor.source_plan().event_sources[0].initial_cursor,
        Some(ADVANCED),
        "a stored cursor outranks the seed, so a restart never rescans from it",
    );
}

/// A resuming trigger without a seed keeps the historical behaviour: an
/// empty store starts at head rather than replaying any history.
#[tokio::test]
async fn a_resuming_trigger_without_a_start_block_still_starts_at_head() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .event_trigger_resuming(1, None),
        )
        .boot()
        .await
        .expect("the example boots alive");

    let sources = booted.supervisor.source_plan().event_sources;
    assert!(
        sources[0].cursor_key.is_some(),
        "resume is on, so the cursor is durable",
    );
    assert_eq!(
        sources[0].initial_cursor, None,
        "no seed and no stored cursor means head, as before",
    );
}
