//! The base block of `bind_host_via_wit_bindgen!` against a stand-in
//! `nexum::host::types`, exercising the fault lift and lower impls the
//! macro emits. Exhaustiveness is compile-checked (the lowering walks
//! the closed `FaultParts` mirror); these tests pin the value mapping
//! case by case.

mod nexum {
    pub mod host {
        /// Stands in for the per-cdylib wit-bindgen `types` output.
        pub mod types {
            #[derive(Clone, Debug, PartialEq, Eq)]
            pub enum Fault {
                Unsupported(String),
                Unavailable(String),
                Denied(String),
                RateLimited,
                Timeout,
                InvalidInput(String),
                Internal(String),
            }

            pub struct Log {
                pub chain_id: u64,
                pub address: Vec<u8>,
                pub topics: Vec<Vec<u8>>,
                pub data: Vec<u8>,
                pub block_hash: Option<Vec<u8>>,
                pub block_number: Option<u64>,
                pub block_timestamp: Option<u64>,
                pub transaction_hash: Option<Vec<u8>>,
                pub transaction_index: Option<u64>,
                pub log_index: Option<u64>,
                pub removed: bool,
            }
        }
    }
}

nexum_sdk::bind_host_via_wit_bindgen!(caps: []);

use nexum::host::types as wire;
use nexum_sdk::host::Fault;

/// Every current fault, paired with its same-named wire case.
fn pairs() -> Vec<(Fault, wire::Fault)> {
    vec![
        (
            Fault::Unsupported("u".into()),
            wire::Fault::Unsupported("u".into()),
        ),
        (
            Fault::Unavailable("a".into()),
            wire::Fault::Unavailable("a".into()),
        ),
        (Fault::Denied("d".into()), wire::Fault::Denied("d".into())),
        (Fault::RateLimited, wire::Fault::RateLimited),
        (Fault::Timeout, wire::Fault::Timeout),
        (
            Fault::InvalidInput("i".into()),
            wire::Fault::InvalidInput("i".into()),
        ),
        (
            Fault::Internal("boom".into()),
            wire::Fault::Internal("boom".into()),
        ),
    ]
}

/// Lower then lift, pinning each direction against the expected value,
/// so a permuted mapping fails even though it would round-trip.
#[test]
fn every_fault_case_round_trips_to_its_same_named_wire_case() {
    for (sdk, wired) in pairs() {
        let lowered = wire::Fault::from(sdk.clone());
        assert_eq!(lowered, wired);
        assert_eq!(Fault::from(lowered), sdk);
    }
}

/// One pair per fault label, all distinct on the wire side, so the pair
/// table cannot silently go stale against the vocabulary.
#[test]
fn the_pair_table_is_total_and_distinct() {
    use strum::VariantNames as _;

    let pairs = pairs();
    assert_eq!(pairs.len(), nexum_world::FaultLabel::VARIANTS.len());
    let wire_cases: std::collections::HashSet<_> = pairs
        .iter()
        .map(|(_, wired)| std::mem::discriminant(wired))
        .collect();
    assert_eq!(wire_cases.len(), pairs.len());
}

#[test]
fn base_block_emits_the_adapter_type() {
    let _adapter = WitBindgenHost;
}

#[test]
fn log_lift_assembles_the_alloy_log() {
    let lifted: nexum_sdk::sol_events::Log = wire::Log {
        chain_id: 1,
        address: vec![0x11; 20],
        topics: vec![vec![0x22; 32]],
        data: vec![1, 2, 3],
        block_hash: Some(vec![0x33; 32]),
        block_number: Some(7),
        block_timestamp: Some(1_000),
        transaction_hash: Some(vec![0x44; 32]),
        transaction_index: Some(1),
        log_index: Some(0),
        removed: false,
    }
    .into();
    assert_eq!(
        lifted.inner.address,
        nexum_sdk::prelude::Address::repeat_byte(0x11)
    );
    assert_eq!(lifted.block_number, Some(7));
}
