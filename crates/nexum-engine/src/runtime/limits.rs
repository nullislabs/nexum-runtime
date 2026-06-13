//! Per-module wasmtime fuel + memory limits. The supervisor refuels
//! the store before every `on_event` so each invocation gets a fresh
//! budget; a module that exhausts fuel traps with `OutOfFuel` and is
//! marked dead.

/// Default fuel budget granted per `on_event` invocation
/// (~ 1 billion WASM instructions). Configurable per-module via
/// `engine.toml` in 0.3.
pub const DEFAULT_FUEL_PER_EVENT: u64 = 1_000_000_000;

/// Default linear-memory cap per module store (64 MiB). Prevents a
/// single runaway module from exhausting process memory. Configurable
/// in 0.3.
pub const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
