//! # tick-recorder (test fixture)
//!
//! Logs `tick <fired-at>` for every schedule tick, through the
//! `on_schedule` handler `#[nexum_sdk::module]` routes to. An optional
//! `hold_ms` config key keeps each schedule and event dispatch open for
//! that long: the sleep is a `wasi:clocks` poll, which the host serves
//! from its own runtime clock, so a test under a paused clock gets a
//! slow handler without waiting for one. An event logs nothing, so a
//! test can keep the engine busy with logs and still read the ticks.
//! Test-only.

// wit_bindgen::generate! expands to host-import shims whose arity matches
// the WIT signatures, which can exceed clippy's too-many-arguments threshold.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]
// wit_bindgen::generate! output carries no doc comments.
#![allow(missing_docs)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nexum::host::{logging, types};

/// Milliseconds each tick's dispatch stays open; zero returns at once.
static HOLD_MS: AtomicU64 = AtomicU64::new(0);

struct TickRecorder;

#[nexum_sdk::module]
impl TickRecorder {
    fn init(config: Vec<(String, String)>) -> Result<(), Fault> {
        let hold_ms = config
            .iter()
            .find(|(key, _)| key == "hold_ms")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        HOLD_MS.store(hold_ms, Ordering::Relaxed);
        logging::log(logging::Level::Info, "tick-recorder init");
        Ok(())
    }

    fn on_event(_log: types::Log) -> Result<(), Fault> {
        hold();
        Ok(())
    }

    fn on_schedule(tick: types::ScheduleTick) -> Result<(), Fault> {
        logging::log(logging::Level::Info, &format!("tick {}", tick.fired_at));
        if hold() {
            logging::log(logging::Level::Info, &format!("held {}", tick.fired_at));
        }
        Ok(())
    }
}

/// Sleep for `hold_ms`; `false` when none is configured.
fn hold() -> bool {
    let hold_ms = HOLD_MS.load(Ordering::Relaxed);
    if hold_ms > 0 {
        std::thread::sleep(Duration::from_millis(hold_ms));
    }
    hold_ms > 0
}
