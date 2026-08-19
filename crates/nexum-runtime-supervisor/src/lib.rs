//! The multi-module supervisor and the event loop that drives it.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![forbid(unsafe_code)]

pub mod error;
mod runtime;
pub mod supervisor;

pub use runtime::event_loop;

#[cfg(feature = "test-utils")]
pub mod test_utils;

pub(crate) use nexum_runtime_api::bindings;
pub(crate) use nexum_runtime_config as engine_config;
pub(crate) use nexum_runtime_manifest as manifest;
