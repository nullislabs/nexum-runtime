//! The RuntimeTypes lattice: one trait naming the core backend seams, so
//! every generic signature takes one parameter.

use crate::host::component::StateStore;

/// Core backend seams a runtime assembly provides. The marker bound is
/// reserved for semver evolution. The chain backend is not a seam.
pub trait RuntimeTypes: crate::sealed::SealedRuntimeTypes + 'static {
    /// Process-wide store vending per-module handles.
    type Store: StateStore<Handle: Send + Sync + 'static> + Clone + Send + Sync + 'static;
}

/// Per-module store handle of a lattice's Store member.
pub type Handle<T> = <<T as RuntimeTypes>::Store as StateStore>::Handle;
