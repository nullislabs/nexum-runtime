//! In-memory [`nexum_sdk::host`] trait implementations plus assertion
//! helpers, so a module can test its logic without wit-bindgen,
//! wasmtime, or a network round-trip.
//!
//! [`MockHost`] composes the per-seam mocks ([`MockChain`],
//! [`MockIdentity`], [`MockLocalStore`], [`MockRemoteStore`],
//! [`MockLogging`]); [`capture_tracing`] records
//! emitted `tracing` events.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![warn(missing_docs)]

mod capture;
mod chain;
mod host;
mod identity;
mod local_store;
mod logging;
mod remote_store;

pub use capture::{CapturedEvent, CapturedEvents, FieldValue, capture_tracing};
pub use chain::{ChainCall, MockChain};
pub use host::MockHost;
pub use identity::{MockIdentity, SignCall, SignPayload};
pub use local_store::{MockLocalStore, TrapStore};
pub use logging::{LogLine, MockLogging};
pub use remote_store::MockRemoteStore;
