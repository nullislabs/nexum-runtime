//! `component.toml` parser and capability enforcement.

#![forbid(unsafe_code)]

mod capabilities;
mod cron;
pub mod error;
mod load;
mod types;

pub use capabilities::{CapabilityRegistry, NamespaceCaps, enforce_capabilities};
pub use cron::{CronError, CronField, CronSchedule};
pub use error::ParseError;
pub use load::load;
pub use types::{ExtensionSections, LoadedManifest, ResourceSection, Trigger};
