//! # topic-parity (build fixture)
//!
//! Compile-only: `subscribes(...)` names the events below, so the macro
//! emits the const parity check against `component.toml`. A drift on either
//! side fails this crate's build.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]
// wit_bindgen::generate! output carries no doc comments.
#![allow(missing_docs)]

use alloy_sol_types::sol;
use nexum::host::{logging, types};

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
}

struct TopicParity;

#[nexum_sdk::module(subscribes(Transfer, Approval))]
impl TopicParity {
    fn on_chain_logs(batch: types::ChainLogs) -> Result<(), Fault> {
        logging::log(
            logging::Level::Info,
            &format!("received {} chain-log entries", batch.logs.len()),
        );
        Ok(())
    }
}
