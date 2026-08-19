//! The [`RuntimeTypes`] lattice over the in-process mocks.

use crate::test_utils::MockStateStore;
use nexum_runtime_api::RuntimeTypes;

/// Lattice binding the mock backends. A type-level marker, only ever named.
pub struct MockTypes;

impl crate::sealed::SealedRuntimeTypes for MockTypes {}

impl RuntimeTypes for MockTypes {
    type State = nexum_runtime_wasm::HostState<Self>;
    type Store = MockStateStore;
}
