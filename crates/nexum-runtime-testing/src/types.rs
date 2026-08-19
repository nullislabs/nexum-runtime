//! The [`RuntimeTypes`] lattice over the in-process mocks.

use crate::MockStateStore;
use nexum_runtime_api::RuntimeTypes;

/// Lattice binding the mock backends. A type-level marker, only ever named.
pub struct MockTypes;

impl nexum_runtime_api::sealed::SealedRuntimeTypes for MockTypes {}

impl RuntimeTypes for MockTypes {
    type State = nexum_runtime_wasm::HostState<Self>;
    type Store = MockStateStore;
}
