//! The RuntimeTypes lattice: one trait naming the assembly's seams, so every
//! generic signature takes one parameter.

use crate::host::component::StateStore;

/// The seams a runtime assembly provides. The marker bound is
/// reserved for semver evolution. The chain backend is not a seam.
pub trait RuntimeTypes: crate::sealed::SealedRuntimeTypes + 'static {
    /// Data held by each module's wasmtime `Store`.
    type State: Send + 'static;
    /// Process-wide store vending per-module handles.
    type Store: StateStore<Handle: Send + Sync + 'static> + Clone + Send + Sync + 'static;
}

/// Per-module store handle of a lattice's Store member.
pub type Handle<T> = <<T as RuntimeTypes>::Store as StateStore>::Handle;
