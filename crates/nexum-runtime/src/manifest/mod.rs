//! `component.toml` parser and capability enforcement.
//!
//! `load` parses and validates a manifest; `capabilities` cross-checks a
//! component's WIT imports against its declared `[dependencies]`; `types`
//! holds the serde shapes and `LoadedManifest`; `error` the error types.
//! A manifest with no `[dependencies]` table is refused at load.

mod capabilities;
mod error;
mod load;
mod types;

pub(crate) use capabilities::enforce_capabilities;
pub use capabilities::{CapabilityRegistry, NamespaceCaps};
pub(crate) use error::{CapabilityError, ParseError};
pub(crate) use load::load;
pub use types::ExtensionSections;
pub(crate) use types::{LoadedManifest, ResourceSection, Trigger};
// CapabilityViolation and the *Section structs are
// reachable through these functions' return / argument types;
// consumers that need to name them directly do so via
// `crate::manifest::error::*` or `::types::*`.
