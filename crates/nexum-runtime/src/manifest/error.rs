//! Error types for manifest parsing and capability enforcement.

use strum::IntoStaticStr;
use thiserror::Error;

use crate::module_id::InvalidModuleName;

/// Errors from loading or validating a manifest.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Failed to read the manifest file from disk.
    #[error("manifest: i/o: {0}")]
    Io(#[from] std::io::Error),
    /// Manifest file was not valid TOML.
    #[error("manifest: parse: {0}")]
    Toml(#[from] toml::de::Error),
    /// A dependency the engine does not recognize as a host capability
    /// or a registered service.
    #[error("manifest: unknown dependency {name:?} in [dependencies] (known: {known})")]
    UnknownCapability {
        /// The unrecognized name.
        name: String,
        /// Comma-joined recognized capability names.
        known: String,
    },
    /// `[component].name` contains `/`, `\`, or `..`, so it could escape
    /// the state directory.
    #[error("manifest: [component].name {0:?} must not contain '/', '\\', or '..'")]
    InvalidModuleName(String),
    /// `[component].name` is missing, empty, or whitespace-only.
    #[error("manifest: [component].name is missing or blank; declare a non-empty name")]
    BlankModuleName,
    /// No `[dependencies]` section; every manifest must declare one.
    #[error(
        "manifest: no [dependencies] section; dependencies are deny-by-default - \
         declare an explicit [dependencies] table (an empty one grants nothing)"
    )]
    MissingCapabilities,
    /// An attribute placed on a dependency that does not take it.
    #[error("manifest: [dependencies].{dependency} does not take `{attribute}`")]
    MisplacedDependencyAttribute {
        /// The dependency carrying the attribute.
        dependency: String,
        /// The attribute name.
        attribute: &'static str,
    },
    #[error("manifest: [component].digest {value:?} is not a valid digest: {source}")]
    InvalidComponentDigest {
        value: String,
        #[source]
        source: crate::digest::DigestParseError,
    },
}

impl From<InvalidModuleName> for ParseError {
    fn from(err: InvalidModuleName) -> Self {
        match err {
            InvalidModuleName::Blank => Self::BlankModuleName,
            InvalidModuleName::UnsafePathComponent(name) => Self::InvalidModuleName(name),
        }
    }
}

/// A capability-bearing WIT import the manifest did not declare.
#[derive(Debug, Error)]
#[error(
    "component imports `{capability}` ({wit_import}) but it is not listed in \
     [dependencies]"
)]
pub struct CapabilityViolation {
    /// Capability name.
    pub capability: String,
    /// Full WIT import name.
    pub wit_import: String,
}

/// A component's WIT imports exceed its declared capabilities.
#[derive(Debug, Error, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilityError {
    /// A gated import was not declared in `[dependencies]`.
    #[error(transparent)]
    Undeclared(#[from] CapabilityViolation),
    /// An unrecognized `wasi:` interface was imported; refused fail-closed.
    #[error(
        "component imports unrecognized WASI interface `{wit_import}`; \
         undeclared WASI is refused by default"
    )]
    UnknownWasi {
        /// Full WIT import name.
        wit_import: String,
    },
}
