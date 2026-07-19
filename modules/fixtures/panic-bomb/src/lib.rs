//! # panic-bomb (test fixture)
//!
//! Installs the nexum-sdk tracing facade (subscriber + panic hook) in
//! `init` and panics on every `on_event`. The hook writes the panic to
//! stderr and forwards it over the host logging call before the trap
//! reaches the supervisor, so one death leaves Stderr, HostInterface,
//! and Panic records on the run.
//!
//! Not a production module. Lives under `modules/fixtures/` so it is
//! obviously test-only.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: [
        "../../../wit/nexum-host",
    ],
    world: "nexum:host/event-module",
    generate_all,
});

use nexum::host::types;

struct PanicBomb;

impl Guest for PanicBomb {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        nexum_sdk::install_host_tracing!();
        tracing::info!("panic-bomb init (will panic)");
        Ok(())
    }

    fn on_event(_event: types::Event) -> Result<(), Fault> {
        panic!("panic-bomb detonated");
    }
}

export!(PanicBomb);
