//! In-memory [`nexum_sdk::host`] trait implementations plus assertion
//! helpers, so a module can test its logic without wit-bindgen,
//! wasmtime, or a network round-trip.
//!
//! [`MockHost`] composes the per-seam mocks ([`MockChain`],
//! [`MockLocalStore`], [`MockLogging`]); [`capture_tracing`] records
//! emitted `tracing` events.
//!
//! The mocks have no manifest, so every seam answers regardless of what
//! `component.toml` declares. `#[nexum_sdk::module]` is what checks the
//! declaration: it binds only the declared adapters, so an undeclared seam
//! does not build. `http` alone is gated at runtime, by the host allowlist.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod capture;
mod chain;
mod host;
mod local_store;
mod logging;

pub use capture::{CapturedEvent, CapturedEvents, FieldValue, capture_tracing};
pub use chain::{ChainCall, MockChain};
pub use host::MockHost;
pub use local_store::{MockLocalStore, TrapStore};
pub use logging::{LogLine, MockLogging};
