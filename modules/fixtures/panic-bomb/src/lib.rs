//! # panic-bomb (test fixture)
//!
//! Installs the nexum-sdk tracing facade (subscriber + panic hook) in
//! `init` and panics on every `on_trigger`. The hook forwards the panic
//! to stderr and the host logging call before the trap reaches the
//! supervisor, so one death leaves Stderr, HostInterface, and Panic
//! records. Test-only.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: [
        "../../../wit/nexum-host",
    ],
    world: "nexum:host/trigger-module",
    generate_all,
});

use nexum::host::types;

nexum_sdk::bind_host_logging_via_wit_bindgen!();

struct PanicBomb;

impl Guest for PanicBomb {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        install_tracing();
        tracing::info!("panic-bomb init (will panic)");
        Ok(())
    }

    fn on_trigger(_trigger: types::Trigger) -> Result<(), Fault> {
        panic!("panic-bomb detonated");
    }
}

export!(PanicBomb);
