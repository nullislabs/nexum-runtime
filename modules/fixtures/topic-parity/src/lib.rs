//! # topic-parity (build fixture)
//!
//! Compile-only: `sol_events(...)` names the events below, so the macro
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

/// Emit one line over the direct `logging` import: no call site, no fields.
fn log_line(level: logging::Level, message: &str) {
    let source = logging::Source {
        target: String::new(),
        file: None,
        line: None,
    };
    logging::log(level, &source, message, &[]);
}

struct TopicParity;

#[nexum_sdk::module(sol_events(Transfer, Approval))]
impl TopicParity {
    fn on_event(log: types::Log) -> Result<(), Fault> {
        log_line(
            logging::Level::Info,
            &format!("event with {} topics", log.topics.len()),
        );
        Ok(())
    }
}
