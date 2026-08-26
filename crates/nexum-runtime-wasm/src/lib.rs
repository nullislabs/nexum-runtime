//! The wasmtime embedding for the Nexum runtime.

#![forbid(unsafe_code)]

mod component;
mod error;
mod extension;
mod fault;
mod http;
mod impls;
mod limits;
mod state;

pub use component::{BuildError, Components, ComponentsBuilder};
pub use extension::attach_wall_clock;
pub use fault::{fault_label, fault_message};
pub use limits::ObservedLimits;
pub use state::HostState;
