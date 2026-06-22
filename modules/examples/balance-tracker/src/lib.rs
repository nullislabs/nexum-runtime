//! # balance-tracker (example Shepherd module)
//!
//! Subscribes to blocks, reads `eth_getBalance(addr)` for every
//! address in `[config].addresses` (comma-separated), persists the
//! last seen value under `balance:{addr}` in local-store, and emits
//! a Warn-level log line when the balance changes by more than
//! `[config].change_threshold` wei since the previous block.
//!
//! ## Module layout
//!
//! - `strategy.rs` holds the pure logic and tests against
//!   `shepherd_sdk::host::Host`. It does not know `wit-bindgen` exists.
//! - `lib.rs` (this file) bridges the per-cdylib wit-bindgen imports
//!   into the trait surface and delegates `init` / `on_event` to
//!   `strategy`.
//!
//! This split is the M3 "host trait + adapter" recipe documented in
//! `docs/tutorial-first-module.md`. Before PR #55 review,
//! balance-tracker called the wit-bindgen free functions directly
//! and could not be unit-tested with `MockHost`; this refactor brings
//! it in line with the other four modules.
//!
//! ## Config
//!
//! ```toml
//! [config]
//! # Comma-separated list of 0x-prefixed 20-byte addresses.
//! addresses = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8,0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
//! # Change threshold in wei; an alert fires when the delta exceeds it.
//! change_threshold = "100000000000000000"  # 0.1 ETH
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: ["../../../wit/nexum-host", "../../../wit/shepherd-cow"],
    world: "shepherd:cow/shepherd",
    generate_all,
});

mod strategy;

use std::sync::OnceLock;

use nexum::host::{logging, types};

// `WitBindgenHost`, `convert_err`, `sdk_err_into_wit`, `convert_level`
// are generated below. Single source of truth in `shepherd-sdk`.
shepherd_sdk::bind_host_via_wit_bindgen!();

static SETTINGS: OnceLock<strategy::Settings> = OnceLock::new();

struct BalanceTracker;

impl Guest for BalanceTracker {
    fn init(config: Vec<(String, String)>) -> Result<(), HostError> {
        let cfg = strategy::parse_config(&config).map_err(sdk_err_into_wit)?;
        logging::log(
            logging::Level::Info,
            &format!(
                "balance-tracker init: {} addresses, threshold={} wei",
                cfg.addresses.len(),
                cfg.change_threshold,
            ),
        );
        // OnceLock::set fails only if already set - in a single-init
        // module that means a re-entry from the supervisor, which is
        // not a hard error; we keep the first parse.
        let _ = SETTINGS.set(cfg);
        Ok(())
    }

    fn on_event(event: types::Event) -> Result<(), HostError> {
        let Some(cfg) = SETTINGS.get() else {
            return Ok(()); // init failed; no-op.
        };
        if let types::Event::Block(block) = event {
            strategy::on_block(&WitBindgenHost, block.chain_id, cfg).map_err(sdk_err_into_wit)?;
        }
        // Logs / Tick / Message are not used by this example.
        Ok(())
    }
}

export!(BalanceTracker);
