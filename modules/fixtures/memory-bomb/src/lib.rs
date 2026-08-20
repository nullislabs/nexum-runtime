//! # memory-bomb (test fixture)
//!
//! Allocates past the default 64 MiB per-module memory cap on every
//! `on_trigger`. The `StoreLimits` refuse the grow, the guest allocator sees
//! the failure and aborts, the supervisor marks the module dead, and other
//! modules keep dispatching. Test-only.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: [
        "../../../wit/nexum-host",
    ],
    world: "nexum:host/trigger-module",
    generate_all,
});

use nexum::host::{logging, types};

struct MemoryBomb;

impl Guest for MemoryBomb {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        // Minimal SDK-free fixture: no tracing subscriber is installed,
        // so log through the raw host binding directly.
        logging::log(
            logging::Level::Info,
            "memory-bomb init (will exhaust memory)",
        );
        Ok(())
    }

    fn on_trigger(_trigger: types::Trigger) -> Result<(), Fault> {
        // The default per-module cap is 64 MiB (`DEFAULT_MEMORY_LIMIT` in
        // `crates/nexum-runtime/src/engine_config/policy.rs`). Asking for
        // 128 MiB
        // makes `memory.grow` return -1, because the host leaves wasmtime's
        // `trap_on_grow_failure` at its default; the trap is this guest's
        // own allocation abort. `black_box` keeps the allocation live so the
        // optimiser cannot eliminate the request.
        let size = 128 * 1024 * 1024;
        let mut buf: Vec<u8> = Vec::with_capacity(size);
        buf.resize(size, 0xab);
        std::hint::black_box(&buf);
        Ok(())
    }
}

export!(MemoryBomb);
