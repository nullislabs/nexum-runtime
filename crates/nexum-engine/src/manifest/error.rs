//! Error types for manifest parsing and capability enforcement.

use super::types::KNOWN_CAPABILITIES;

/// Errors returned while loading or validating a manifest.
#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    UnknownCapability(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "manifest: i/o: {e}"),
            Self::Toml(e) => write!(f, "manifest: parse: {e}"),
            Self::UnknownCapability(name) => write!(
                f,
                "manifest: unknown capability {:?} in [capabilities].required (known: {})",
                name,
                KNOWN_CAPABILITIES.join(", ")
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Error returned when a component's WIT imports exceed its declared capabilities.
#[derive(Debug)]
pub struct CapabilityViolation {
    /// Capability name (e.g. `"remote-store"`).
    pub capability: String,
    /// Full WIT import name as it appeared in the component (e.g.
    /// `"nexum:host/remote-store@0.2.0"`).
    pub wit_import: String,
}

impl std::fmt::Display for CapabilityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "component imports `{}` ({}) but it is not listed in \
             [capabilities].required or [capabilities].optional",
            self.capability, self.wit_import
        )
    }
}

impl std::error::Error for CapabilityViolation {}
