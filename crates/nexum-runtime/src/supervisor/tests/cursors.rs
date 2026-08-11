//! Chain-log filters, log projection, cursor keys and persistence.

use super::*;

/// Data-compat guard: the persisted progress marker keys on the numeric
/// chain id, so a named chain still yields `last_dispatched_block:11155111`.
#[test]
fn progress_marker_key_uses_numeric_chain_id() {
    let chain = Chain::from_id(11_155_111);
    assert_eq!(progress_key(chain), "last_dispatched_block:11155111");
}

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
fn project_chain_log_preserves_mined_log() {
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

    let projected = nexum::host::types::ChainLog::from(&log);

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
fn project_chain_log_leaves_pending_fields_none() {
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

    let projected = nexum::host::types::ChainLog::from(&log);

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
fn cursor_record_is_per_subscription() {
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
