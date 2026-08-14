use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

use super::chain::RpcEndpointError;

/// Errors surfaced by [`load_or_default`](super::load_or_default).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineConfigError {
    /// The file exists but reading it failed.
    #[error("read engine config: {0}")]
    Io(#[from] std::io::Error),
    /// Syntax or an unknown key; value refusals surface as typed variants.
    #[error("parse engine config: {0}")]
    Toml(#[from] toml::de::Error),
    /// Refused before the TOML parse.
    #[error("engine config env-var substitution failed: {0}")]
    Substitute(#[from] EnvVarError),
    /// Refused rather than dropped: an ignored table loses a chain silently.
    #[error("engine config: [chains] key {key:?} is not a chain id or known chain name")]
    InvalidChainKey {
        /// The key as written.
        key: String,
    },
    /// A zero in a numeric field whose mechanism a zero would disable.
    #[error("engine config: {field} must not be 0")]
    ZeroField {
        /// TOML path of the refused field.
        field: String,
    },
    /// Refused at load, not at the first dial.
    #[error("engine config: chains.{key}.rpc_url: {source}")]
    InvalidRpcUrl {
        /// The `[chains]` key.
        key: String,
        /// Why the URL refused.
        #[source]
        source: RpcEndpointError,
    },
    /// Refused rather than ignored: a dead knob reads as an applied cap.
    #[error("engine config: {key} is retired; set {replacement}")]
    RetiredKey {
        /// The retired TOML path.
        key: &'static str,
        /// The `[policy]` path that replaces it.
        replacement: &'static str,
    },
    /// `id` keys `[policy.component]`, so it cannot be blank.
    #[error("engine config: [[modules]] entry {} needs a non-empty id", path.display())]
    EmptyComponentId {
        /// The entry's component path.
        path: PathBuf,
    },
    /// A second claimant would make the policy join ambiguous.
    #[error("engine config: [[modules]].id {id:?} is claimed twice")]
    DuplicateComponentId {
        /// The doubly claimed id.
        id: String,
    },
    /// A policy row that binds to nothing is a typo, and an unapplied
    /// narrowing row fails open.
    #[error("engine config: [policy.component.{id}] matches no [[modules]].id")]
    UnknownPolicyComponent {
        /// The row's key as written.
        id: String,
    },
    /// Refused rather than skipped: a dropped deny entry fails open.
    #[error("engine config: policy.http_deny entry {entry:?} is not an IP address or CIDR block")]
    InvalidHttpDeny {
        /// The entry as written.
        entry: String,
    },
    /// Refused rather than dropped: an ignored row loses an authorization
    /// and its claimant then refuses at load with the wrong message.
    #[error(
        "engine config: [implements] key {key:?} is not an interface track \
         (namespace:package/interface@major, or @0.minor below 1.0)"
    )]
    InvalidInterfaceTrack {
        /// The key as written.
        key: String,
    },
    /// A row binding to nothing is a typo, and an unapplied binding fails
    /// closed at load with a message that points away from the typo.
    #[error(
        "engine config: [implements].{interface:?} names component {id:?}, \
         which matches no [[modules]].id"
    )]
    UnknownImplementsComponent {
        /// The row's key as written.
        interface: String,
        /// The dangling component value.
        id: String,
    },
    /// Refused at load, as the `[component].digest` grammar is.
    #[error("engine config: [implements].{interface:?} digest {value:?}: {source}")]
    InvalidImplementerDigest {
        /// The row's key as written.
        interface: String,
        /// The digest as written.
        value: String,
        /// Why the digest refused.
        #[source]
        source: crate::digest::DigestParseError,
    },
}

/// Errors from `${VAR}` substitution in `engine.toml`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvVarError {
    /// A referenced variable is absent from the process environment.
    /// Substitution refuses rather than expanding to empty, because an
    /// empty RPC URL fails later and further from the cause.
    #[error(
        "environment variable `{name}` referenced via ${{{name}}} in engine.toml but not set. \
         Export it before launching the engine (e.g. via a `.env` file consumed by `docker compose`)."
    )]
    Missing {
        /// The variable as referenced.
        name: String,
    },
    /// The name inside `${...}` is not a shell-style variable name. The
    /// message guesses an upper-case spelling, which is the usual slip.
    #[error(
        "invalid env var name `{name}` inside ${{...}} in engine.toml - names must match \
         [A-Z_][A-Z0-9_]*. Typo, or did you mean `${{{name_upper}}}`?",
        name_upper = name.to_uppercase()
    )]
    InvalidName {
        /// The rejected name, as written.
        name: String,
    },
    /// A `${` with no closing brace before the end of the file.
    #[error(
        "unclosed `${{` at byte offset {offset} in engine.toml - every `${{` needs a matching `}}`."
    )]
    Unclosed {
        /// Byte offset of the opening `${` that never closed.
        offset: usize,
    },
}

/// A configured zero, named by its TOML path.
pub(super) fn zero_field(field: &str) -> EngineConfigError {
    EngineConfigError::ZeroField {
        field: field.to_owned(),
    }
}

/// Override-or-default, proving the resolution in the type; a zero
/// override refuses, naming `field`.
pub(super) fn nonzero_u64(
    field: &str,
    value: Option<u64>,
    default: NonZeroU64,
) -> Result<NonZeroU64, EngineConfigError> {
    match value {
        Some(v) => NonZeroU64::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// As [`nonzero_u64`], for `u32` knobs.
pub(super) fn nonzero_u32(
    field: &str,
    value: Option<u32>,
    default: NonZeroU32,
) -> Result<NonZeroU32, EngineConfigError> {
    match value {
        Some(v) => NonZeroU32::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// As [`nonzero_u64`], for `usize` knobs.
pub(super) fn nonzero_usize(
    field: &str,
    value: Option<usize>,
    default: NonZeroUsize,
) -> Result<NonZeroUsize, EngineConfigError> {
    match value {
        Some(v) => NonZeroUsize::new(v).ok_or_else(|| zero_field(field)),
        None => Ok(default),
    }
}

/// Second-denominated knob resolved to a `Duration`, zero refused.
pub(super) fn nonzero_secs(
    field: &str,
    value: Option<u64>,
    default: Duration,
) -> Result<Duration, EngineConfigError> {
    match value {
        Some(0) => Err(zero_field(field)),
        Some(secs) => Ok(Duration::from_secs(secs)),
        None => Ok(default),
    }
}
