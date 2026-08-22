//! Seam traits between the runtime engine and its backends and extensions.

#![forbid(unsafe_code)]

pub mod bindings;
mod builder;
mod clock;
mod extension;
mod runtime_types;
mod state;

/// Markers reserved for semver evolution of [`RuntimeTypes`]: implement
/// alongside the trait.
#[doc(hidden)]
pub mod sealed {
    pub trait SealedRuntimeTypes {}
}

pub use builder::{BuilderContext, ComponentBuilder};
pub use clock::WasiClockOverride;
pub use extension::{
    Extension, ExtensionDelivery, ExtensionError, ExtensionSource, HostWallClock, SourceContext,
};
pub use runtime_types::{Handle, RuntimeTypes};
pub use state::{
    EntryPage, ListQuery, MAX_APPLY_OPS, MAX_APPLY_VALUE_BYTES, MAX_LIST_LIMIT,
    MAX_LIST_RESPONSE_BYTES, MAX_LIST_SCAN_LIMIT, StateHandle, StateStore, StoreError, ValueFilter,
    WriteOp,
};

/// The error an implementor-facing seam takes, so implementing one needs
/// no `anyhow` dependency.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
