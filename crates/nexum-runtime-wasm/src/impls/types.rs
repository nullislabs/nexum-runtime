//! `nexum:host/types` is a type-only interface (no functions). The
//! generated trait is empty; we just provide the marker impl.

use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_api::bindings::nexum;

use crate::state::HostState;

impl<T: RuntimeTypes> nexum::host::types::Host for HostState<T> {}
