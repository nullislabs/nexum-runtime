//! Host-side backends for the `nexum:host` / `shepherd:cow`
//! interfaces.
//!
//! Each submodule owns one capability. The trait impls in `main.rs`
//! stay thin: they validate inputs, dispatch to the backend, and
//! project the backend's typed error onto the bindgen-generated
//! `HostError`. Keeping the backends pure (no bindgen types) means
//! each can be unit-tested without spinning up a wasmtime store.

pub mod cow_orderbook;
pub mod local_store_redb;
pub mod provider_pool;
