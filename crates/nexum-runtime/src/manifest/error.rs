//! Error types for manifest parsing and capability enforcement.

use strum::{IntoStaticStr, VariantNames};
use thiserror::Error;

use crate::module_id::InvalidModuleName;

/// Errors from loading or validating a manifest.
// `IntoStaticStr`: the snake_case variant name is the stable per-class
// label a metric or log field carries; `VariantNames` lets the
// label-set test enumerate without a value.
#[derive(Debug, Error, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
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
    /// `[component].name` has leading or trailing whitespace.
    #[error("manifest: [component].name {0:?} must not have leading or trailing whitespace")]
    UntrimmedModuleName(String),
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
    #[error("manifest: [component].provides {value:?} is not an interface id: {source}")]
    InvalidInterfaceId {
        value: String,
        #[source]
        source: crate::interface_id::InvalidInterfaceId,
    },
    /// A dependency's `interface` value that is not a compatibility track.
    /// A full version is refused too: a consumer asks for compatibility,
    /// and the provider's exact version is the provider's business.
    #[error("manifest: [dependencies].{dependency}: {source}")]
    InvalidInterfaceTrack {
        /// The dependency carrying the value.
        dependency: String,
        #[source]
        source: crate::interface_id::InvalidInterfaceTrack,
    },
    /// An interface dependency whose alias is a capability name. The
    /// capability table resolves first, so the alias would shadow it.
    #[error(
        "manifest: [dependencies].{dependency} declares an interface under a \
         capability's name; rename the alias (capabilities: {known})"
    )]
    AliasShadowsCapability {
        /// The colliding alias.
        dependency: String,
        /// Comma-joined recognized capability names.
        known: String,
    },
    /// A dependency naming a component where an interface belongs.
    #[error(
        "manifest: [dependencies].{dependency} names a component, not an \
         interface; that component provides {interface}, so write \
         {dependency} = {{ interface = \"{interface}\" }}"
    )]
    DependencyNamesComponent {
        /// The component name written as a dependency.
        dependency: String,
        /// The track the named component provides.
        interface: crate::interface_id::InterfaceTrack,
    },
    /// An interface dependency the declaring component's own `provides`
    /// claim satisfies. The prepass refuses a second claimant of one
    /// track, so the only provider is the consumer itself.
    #[error(
        "manifest: [dependencies].{dependency} depends on interface \
         {interface}, which this component itself provides"
    )]
    SelfInterfaceDependency {
        /// The declaring alias.
        dependency: String,
        /// The requested track.
        interface: crate::interface_id::InterfaceTrack,
    },
    /// A dependency on a compatibility track no loaded component provides.
    #[error(
        "manifest: [dependencies].{dependency} depends on interface \
         {interface} but no loaded component provides it (provided: {provided})"
    )]
    InterfaceNotProvided {
        /// The declaring alias.
        dependency: String,
        /// The requested track.
        interface: crate::interface_id::InterfaceTrack,
        /// Comma-joined provided tracks, or `none`.
        provided: String,
    },
    /// A `[[subscription]]` table without a string `kind`.
    #[error("manifest: [[subscription]] table {index} must declare a string `kind`")]
    MissingSubscriptionKind {
        /// 1-based position among the `[[subscription]]` tables.
        index: usize,
    },
    /// A core-kind `[[subscription]]` table whose shape does not match
    /// its kind.
    #[error("manifest: invalid {kind:?} subscription ([[subscription]] table {index}): {source}")]
    InvalidSubscription {
        /// 1-based position among the `[[subscription]]` tables.
        index: usize,
        /// The declared core kind.
        kind: String,
        #[source]
        source: toml::de::Error,
    },
    /// A chain-log `address` that is not 20-byte hex.
    #[error("manifest: invalid chain-log address {value:?}: {source}")]
    InvalidChainLogAddress {
        /// The address as written.
        value: String,
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// A chain-log `event_signature` that is not 32-byte hex.
    #[error("manifest: invalid topic {value:?}: {source}")]
    InvalidChainLogTopic {
        /// The topic as written.
        value: String,
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// An extension-kind subscription filter with a non-string value.
    #[error("manifest: subscription filter `{key}` must be a string")]
    NonStringSubscriptionFilter {
        /// The filter key.
        key: String,
    },
}

impl From<InvalidModuleName> for ParseError {
    fn from(err: InvalidModuleName) -> Self {
        match err {
            InvalidModuleName::Blank => Self::BlankModuleName,
            InvalidModuleName::UnsafePathComponent(name) => Self::InvalidModuleName(name),
            InvalidModuleName::Untrimmed(name) => Self::UntrimmedModuleName(name),
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
// `IntoStaticStr`: the snake_case variant name is the `error_kind` label;
// `VariantNames` lets the label-set test enumerate without a value.
#[derive(Debug, Error, IntoStaticStr, VariantNames)]
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
