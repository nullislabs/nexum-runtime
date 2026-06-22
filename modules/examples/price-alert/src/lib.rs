//! # price-alert (example Shepherd module)
//!
//! Polls a Chainlink price oracle on every new block (throttled by
//! `every_n_blocks`) and emits a Warn-level log when the price
//! crosses a config-supplied threshold.
//!
//! ## Module layout
//!
//! - `strategy.rs` holds the pure logic and tests against
//!   `shepherd_sdk::host::Host`. It does not know `wit-bindgen`
//!   exists.
//! - `lib.rs` (this file) bridges the per-cdylib wit-bindgen imports
//!   into the trait surface and delegates `init` / `on_event` to
//!   `strategy`.
//!
//! This split is the M3 "host trait + adapter" recipe documented in
//! `docs/tutorial-first-module.md`.

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
// are generated below. The macro is the single source of truth for
// the ~80 lines of wit-bindgen ↔ SDK glue every module shares.
shepherd_sdk::bind_host_via_wit_bindgen!();

static SETTINGS: OnceLock<strategy::Settings> = OnceLock::new();

struct PriceAlert;

impl Guest for PriceAlert {
    fn init(config: Vec<(String, String)>) -> Result<(), HostError> {
        let cfg = strategy::parse_config(&config).map_err(sdk_err_into_wit)?;
        logging::log(
            logging::Level::Info,
            &format!(
                "price-alert init: oracle={:#x} threshold={} direction={:?} every_n_blocks={}",
                cfg.oracle_address, cfg.threshold_scaled, cfg.direction, cfg.every_n_blocks,
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
            strategy::on_block(&WitBindgenHost, block.chain_id, cfg, block.number)
                .map_err(sdk_err_into_wit)?;
        }
        // Logs / Tick / Message are not used by this example.
        Ok(())
    }
}

export!(PriceAlert);
