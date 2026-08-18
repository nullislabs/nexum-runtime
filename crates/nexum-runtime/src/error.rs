//! The one typed error the public runtime surface returns.
//!
//! The `error_kind` label set is closed, and the test below pins it.

use thiserror::Error;

pub use crate::builder::LaunchRefusal;
pub use crate::engine_config::{EngineConfigError, EnvVarError, RpcEndpointError};
pub use crate::host::component::{BuildError, StoreError};
pub use crate::host::extension::ExtensionError;
pub use crate::host::local_store_redb::StorageError;
pub use crate::host::provider_pool::PoolError;
pub use crate::manifest::error::{CapabilityError, CapabilityViolation, ParseError};
pub use crate::supervisor::load::LoadRefusal;
pub use crate::supervisor::prepass::BootRefusal;
pub use nexum_primitives::digest::{DigestMismatch, DigestParseError};
pub use nexum_primitives::interface_id::{InvalidInterfaceId, InvalidInterfaceTrack};
pub use nexum_primitives::module_id::InvalidModuleName;
pub use semver::Error as SemverError;
pub use url::ParseError as UrlParseError;

/// The error an implementor-facing seam takes, so implementing one needs
/// no `anyhow` dependency.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A wasmtime seam failure: engine, linker, compile, store, instantiate,
/// a host call trapping under `init`, and the local-store namespace open.
///
/// The field stays private because `wasmtime::Error` is `anyhow::Error`.
#[derive(Debug)]
pub struct EngineRefusal(anyhow::Error);

impl EngineRefusal {
    pub(crate) fn new(inner: impl Into<anyhow::Error>) -> Self {
        Self(inner.into())
    }
}

impl std::fmt::Display for EngineRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for EngineRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// A refusal from the boot path, or a frame wrapping one.
#[derive(Debug, Error)]
#[cfg_attr(test, derive(strum::VariantNames))]
pub enum RuntimeError {
    /// Refused before any compile: manifests, namespace claims, and the
    /// configured-chains gate.
    #[error(transparent)]
    Boot(#[from] BootRefusal),
    /// Refused at admission, ahead of instantiation.
    #[error(transparent)]
    Load(#[from] LoadRefusal),
    /// Refused by the launcher around the supervisor boot.
    #[error(transparent)]
    Launch(#[from] LaunchRefusal),
    /// The component's WIT imports exceed its declared capabilities.
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    /// The artifact's bytes hash differently from the manifest's pin.
    #[error(transparent)]
    Digest(#[from] DigestMismatch),
    /// The engine config failed to load or validate.
    #[error(transparent)]
    Config(#[from] EngineConfigError),
    /// A chain provider failed to open at boot.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// A backend slot builder failed.
    #[error(transparent)]
    Backend(BuildError),
    /// An add-on failed to install.
    #[error(transparent)]
    AddOn(BoxError),
    /// An extension hook refused.
    #[error(transparent)]
    Extension(#[from] ExtensionError),
    /// A frame naming where the wrapped error arose.
    #[error("{context}")]
    Context {
        /// Outermost frame first, anyhow-style.
        context: String,
        /// The wrapped error.
        source: Box<RuntimeError>,
    },
    /// A wasmtime seam failed.
    #[error(transparent)]
    Engine(#[from] EngineRefusal),
}

impl RuntimeError {
    /// The `error_kind` label the boot-refusal counter records. `None`
    /// goes uncounted.
    pub fn error_kind(&self) -> Option<&'static str> {
        match self {
            Self::Context { source, .. } => source.error_kind(),
            Self::Boot(refusal) => Some(boot_kind(refusal)),
            Self::Load(refusal) => Some(refusal.into()),
            Self::Launch(refusal) => launch_kind(refusal),
            Self::Capability(violation) => Some(violation.into()),
            Self::Digest(_) => Some("digest_mismatch"),
            Self::Config(_) | Self::Pool(_) | Self::Backend(_) | Self::AddOn(_) => None,
            Self::Extension(err) => chained_kind(std::error::Error::source(err)),
            Self::Engine(engine) => engine.0.chain().find_map(untyped_kind),
        }
    }

    /// The error under any context frames, as a downcastable value.
    pub fn cause(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Self::Boot(refusal) => refusal,
            Self::Load(refusal) => refusal,
            Self::Launch(refusal) => refusal,
            Self::Capability(violation) => violation,
            Self::Digest(mismatch) => mismatch,
            Self::Config(err) => err,
            Self::Pool(err) => err,
            Self::Backend(err) => err,
            Self::AddOn(err) => &**err,
            Self::Extension(err) => err,
            Self::Context { source, .. } => source.cause(),
            Self::Engine(engine) => &*engine.0,
        }
    }

    pub(crate) fn context(self, context: String) -> Self {
        Self::Context {
            context,
            source: Box::new(self),
        }
    }
}

/// Sees through a slot that boxed a `RuntimeError`, so a chain-connect
/// failure arrives as [`RuntimeError::Pool`] and not nested one deeper.
impl From<BuildError> for RuntimeError {
    fn from(err: BuildError) -> Self {
        fn see_through(source: BoxError, slot: fn(BoxError) -> BuildError) -> RuntimeError {
            match source.downcast::<RuntimeError>() {
                Ok(nested) => *nested,
                Err(source) => RuntimeError::Backend(slot(source)),
            }
        }
        match err {
            BuildError::Chain(source) => see_through(source, BuildError::Chain),
            BuildError::Store(source) => see_through(source, BuildError::Store),
            BuildError::Logs(source) => see_through(source, BuildError::Logs),
        }
    }
}

/// The wrapped `ParseError` names the manifest refusal class; the flat
/// `manifest` label would collapse the classes into one.
fn boot_kind(refusal: &BootRefusal) -> &'static str {
    match refusal {
        BootRefusal::Manifest(parse) => parse.into(),
        refusal => refusal.into(),
    }
}

/// `EventLoopGone` is raised by `RuntimeHandle::wait` after a successful
/// boot, so it carries no boot-refusal label and is never counted.
fn launch_kind(refusal: &LaunchRefusal) -> Option<&'static str> {
    match refusal {
        LaunchRefusal::EventLoopGone => None,
        refusal => Some(refusal.into()),
    }
}

fn chained_kind(mut cause: Option<&(dyn std::error::Error + 'static)>) -> Option<&'static str> {
    while let Some(current) = cause {
        if let Some(kind) = untyped_kind(current) {
            return Some(kind);
        }
        cause = current.source();
    }
    None
}

/// Recovers the class of a typed refusal an embedder's hook boxed. No
/// in-tree boot path reaches it.
fn untyped_kind(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    if let Some(error) = cause.downcast_ref::<RuntimeError>() {
        return error.error_kind();
    }
    if let Some(refusal) = cause.downcast_ref::<BootRefusal>() {
        return Some(boot_kind(refusal));
    }
    if let Some(refusal) = cause.downcast_ref::<LoadRefusal>() {
        return Some(refusal.into());
    }
    if let Some(refusal) = cause.downcast_ref::<LaunchRefusal>() {
        return launch_kind(refusal);
    }
    if let Some(violation) = cause.downcast_ref::<CapabilityError>() {
        return Some(violation.into());
    }
    cause
        .downcast_ref::<DigestMismatch>()
        .map(|_| "digest_mismatch")
}

/// `with_context` for the fallibles on the boot path.
pub(crate) trait RefusalContext<T> {
    fn with_refusal_context(self, context: impl FnOnce() -> String) -> Result<T, RuntimeError>;
}

impl<T, E: Into<RuntimeError>> RefusalContext<T> for Result<T, E> {
    fn with_refusal_context(self, context: impl FnOnce() -> String) -> Result<T, RuntimeError> {
        self.map_err(|err| err.into().context(context()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use strum::VariantNames;

    use super::*;
    use crate::manifest::ParseError;

    /// An operator contract: a change here is a deliberate diff.
    const PINNED_LABELS: &[&str] = &[
        // BootRefusal, with `manifest` split into the ParseError classes.
        "namespace_claimed",
        "manifest_not_found",
        "manifest_missing",
        "unconfigured_chain_defaulted",
        "unconfigured_chain",
        "total_memory_exceeded",
        // ParseError.
        "io",
        "toml",
        "unknown_capability",
        "invalid_module_name",
        "blank_module_name",
        "untrimmed_module_name",
        "missing_capabilities",
        "misplaced_dependency_attribute",
        "invalid_component_digest",
        "missing_trigger_kind",
        "invalid_trigger",
        "invalid_event_address",
        "invalid_event_topic",
        "non_string_trigger_filter",
        // LoadRefusal.
        "section_unclaimed",
        "extension_namespace_claimed",
        "trigger_kind_claimed",
        "section_claimed",
        "unknown_trigger_kind",
        "digest_unpinned",
        "capability_not_permitted",
        "chain_trigger_not_permitted",
        // LaunchRefusal, less the wait-time `event_loop_gone`, which is
        // raised after a successful boot and never counted.
        "nothing_to_run",
        "all_dead_override",
        "all_dead_configured",
        "dead_hold_triggers",
        // CapabilityError.
        "undeclared",
        "unknown_wasi",
        // DigestMismatch.
        "digest_mismatch",
    ];

    /// Keyed by arm name from `RuntimeError::VARIANTS`, so a new arm
    /// panics until it names its labels.
    fn arm_labels(arm: &str) -> Vec<&'static str> {
        match arm {
            "Boot" => BootRefusal::VARIANTS
                .iter()
                .copied()
                // `boot_kind` maps the Manifest arm to the wrapped
                // ParseError class, never to the flat `manifest` label.
                .filter(|v| *v != "manifest")
                .chain(ParseError::VARIANTS.iter().copied())
                .collect(),
            "Load" => LoadRefusal::VARIANTS.to_vec(),
            // `launch_kind` withholds the wait-time `event_loop_gone`.
            "Launch" => LaunchRefusal::VARIANTS
                .iter()
                .copied()
                .filter(|v| *v != "event_loop_gone")
                .collect(),
            "Capability" => CapabilityError::VARIANTS.to_vec(),
            "Digest" => vec!["digest_mismatch"],
            // Context delegates to its source; the arms outside the
            // refusal vocabulary recover a label from another arm's table
            // or none at all.
            "Context" | "Config" | "Pool" | "Backend" | "AddOn" | "Extension" | "Engine" => {
                Vec::new()
            }
            arm => panic!("arm {arm} has no label table; add it here and extend PINNED_LABELS"),
        }
    }

    fn derived_labels() -> Vec<&'static str> {
        RuntimeError::VARIANTS
            .iter()
            .flat_map(|arm| arm_labels(arm))
            .collect()
    }

    fn unknown_wasi() -> CapabilityError {
        CapabilityError::UnknownWasi {
            wit_import: "wasi:sockets/tcp@0.2.0".to_owned(),
        }
    }

    /// Keyed like [`arm_labels`], and panics on an arm absent here.
    fn representative(arm: &str) -> (RuntimeError, Option<&'static str>) {
        let manifest_missing = || BootRefusal::ManifestMissing {
            component: PathBuf::from("orphan.wasm"),
        };
        match arm {
            "Boot" => (manifest_missing().into(), Some("manifest_missing")),
            "Load" => (
                LoadRefusal::SectionClaimed { section: "venue" }.into(),
                Some("section_claimed"),
            ),
            "Launch" => (LaunchRefusal::NothingToRun.into(), Some("nothing_to_run")),
            "Capability" => (unknown_wasi().into(), Some("unknown_wasi")),
            "Digest" => (
                DigestMismatch {
                    path: PathBuf::from("pinned.wasm"),
                    pin: nexum_primitives::digest::DigestPin::Author,
                    declared: nexum_primitives::digest::ContentDigest::of_bytes(b"declared"),
                    actual: nexum_primitives::digest::ContentDigest::of_bytes(b"actual"),
                }
                .into(),
                Some("digest_mismatch"),
            ),
            "Config" => (
                EngineConfigError::ZeroField {
                    field: "limits.max_fuel_per_dispatch".to_owned(),
                }
                .into(),
                None,
            ),
            "Pool" => (PoolError::Timeout.into(), None),
            "Backend" => (
                BuildError::Logs(Box::from("log pipeline gone")).into(),
                None,
            ),
            "AddOn" => (
                RuntimeError::AddOn(Box::from("recorder already installed")),
                None,
            ),
            "Extension" => (
                ExtensionError::link("acme", "linker rejected the hook").into(),
                None,
            ),
            "Engine" => (
                EngineRefusal::new(anyhow::anyhow!("engine gone")).into(),
                None,
            ),
            "Context" => (
                RuntimeError::from(manifest_missing())
                    .context("load module orphan.wasm".to_owned()),
                Some("manifest_missing"),
            ),
            arm => panic!("arm {arm} has no representative; add one"),
        }
    }

    #[test]
    fn the_error_kind_label_set_is_closed_and_pinned() {
        let derived = derived_labels();
        let derived_set: BTreeSet<&str> = derived.iter().copied().collect();
        assert_eq!(
            derived_set.len(),
            derived.len(),
            "two refusal classes share one error_kind label",
        );
        let pinned: BTreeSet<&str> = PINNED_LABELS.iter().copied().collect();
        assert_eq!(
            pinned.len(),
            PINNED_LABELS.len(),
            "the pinned list repeats a label",
        );
        assert_eq!(
            derived_set, pinned,
            "the error_kind label set drifted from the pin; \
             a label change is an operator contract change",
        );
    }

    #[test]
    fn every_arm_maps_into_the_pinned_set() {
        for arm in RuntimeError::VARIANTS {
            let (error, expected) = representative(arm);
            assert_eq!(error.error_kind(), expected, "arm {arm}: {error}");
            let Some(kind) = expected else {
                continue;
            };
            assert!(
                PINNED_LABELS.contains(&kind),
                "arm {arm}: {kind} not pinned"
            );
            // Context delegates, so its label lives in its source's table.
            if *arm != "Context" {
                assert!(
                    arm_labels(arm).contains(&kind),
                    "arm {arm}: {kind} escapes the arm's table",
                );
            }
        }
    }

    #[test]
    fn context_frames_keep_the_root_label() {
        let error = RuntimeError::from(BootRefusal::ManifestMissing {
            component: PathBuf::from("orphan.wasm"),
        })
        .context("load module orphan.wasm".to_owned());
        assert_eq!(error.error_kind(), Some("manifest_missing"));
    }

    #[test]
    fn a_manifest_refusal_counts_under_its_parse_class() {
        let error = RuntimeError::from(BootRefusal::Manifest(ParseError::MissingCapabilities));
        assert_eq!(error.error_kind(), Some("missing_capabilities"));
        let error = RuntimeError::from(BootRefusal::Manifest(ParseError::BlankModuleName));
        assert_eq!(error.error_kind(), Some("blank_module_name"));
    }

    #[test]
    fn a_boxed_refusal_in_a_build_slot_converts_to_its_own_arm() {
        let boxed: BoxError = Box::new(RuntimeError::from(PoolError::Timeout));
        let converted = RuntimeError::from(BuildError::Chain(boxed));
        assert!(
            matches!(converted, RuntimeError::Pool(_)),
            "expected the nested Pool refusal, got {converted:?}",
        );
        let foreign = RuntimeError::from(BuildError::Chain(Box::from("connect refused")));
        assert!(
            matches!(foreign, RuntimeError::Backend(BuildError::Chain(_))),
            "a foreign slot failure keeps its Backend wrap, got {foreign:?}",
        );
    }

    #[test]
    fn a_typed_refusal_inside_a_foreign_wrap_keeps_its_label() {
        let engine_wrapped = EngineRefusal::new(
            anyhow::Error::new(unknown_wasi()).context("install refused for module.wasm"),
        );
        assert_eq!(
            RuntimeError::from(engine_wrapped).error_kind(),
            Some("unknown_wasi"),
            "the engine seam keeps the class it smuggled through anyhow",
        );
        let admit_refused = ExtensionError::admit("module", unknown_wasi());
        assert_eq!(
            RuntimeError::from(admit_refused).error_kind(),
            Some("unknown_wasi"),
            "the extension seam keeps the class its hook boxed",
        );
        assert_eq!(
            RuntimeError::from(EngineRefusal::new(anyhow::anyhow!("engine gone"))).error_kind(),
            None,
            "an untyped failure is counted under no kind rather than a wrong one",
        );
    }
}
