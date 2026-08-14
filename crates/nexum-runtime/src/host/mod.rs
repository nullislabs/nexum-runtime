//! Host-side backends for the `nexum:host` interfaces, plus the per-module
//! [`state::HostState`] and the WIT `Host` trait impls.
//!
//! [`provider_pool`] and [`local_store_redb`] are the capability backends;
//! [`component`] is the backend-trait seam; [`extension`] wires in domain
//! extensions; [`http`] gates outgoing wasi:http; [`logs`] is the module-log
//! pipeline; [`error`] is the construction funnel for the WIT `chain-error`
//! shapes; [`fault`] projects a `Fault` into log and metric fields.

pub mod component;
pub mod error;
pub mod extension;
pub mod fault;
pub mod http;
mod impls;
pub mod local_store_redb;
pub mod logs;
pub mod provider_pool;
pub mod state;
