//! The [`RuntimeTypes`] lattice over the in-process mocks.

use crate::host::component::RuntimeTypes;
use crate::test_utils::MockStateStore;

/// Lattice binding the mock backends. A type-level marker, only ever named.
pub struct MockTypes;

impl crate::sealed::SealedRuntimeTypes for MockTypes {}

impl RuntimeTypes for MockTypes {
    type Store = MockStateStore;
}
