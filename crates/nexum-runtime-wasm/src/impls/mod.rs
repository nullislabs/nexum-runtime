//! `Host` trait impls for [`crate::state::HostState`], one file per WIT
//! interface: dispatch glue to the capability backends.

mod chain;
mod identity;
mod local_store;
mod logging;
mod remote_store;
mod types;
