//! The one refusal value the boot path returns.
//!
//! [`Refusal`] composes the typed refusal vocabulary from the supervisor,
//! the launcher, and the manifest layer, so the boot-refusal counter
//! matches on a value instead of downcasting an `anyhow` chain. The
//! `error_kind` metric label set is closed: [`Refusal::error_kind`] is
//! exhaustive over the arms, and the test below pins the resulting set.
//! The one untyped seam is [`Refusal::Other`]: the extension admit hooks
//! return `anyhow::Error`, so a typed refusal they carry is recovered
//! best-effort rather than lost.

use thiserror::Error;

use crate::builder::LaunchRefusal;
use crate::digest::DigestMismatch;
use crate::manifest::CapabilityError;
use crate::supervisor::{BootRefusal, LoadRefusal};

/// A refusal from the boot path, or the context and untyped failures
/// wrapping one.
///
/// Arms delegate their `Display` to the wrapped type, so the operator
/// reads the same wording the wrapped refusal carries.
#[derive(Debug, Error)]
#[cfg_attr(test, derive(strum::VariantNames))]
#[non_exhaustive]
pub enum Refusal {
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
    /// A context frame naming where the wrapped refusal arose.
    #[error("{context}")]
    Context {
        /// The frame's message, outermost first, anyhow-style.
        context: String,
        /// The refusal the frame wraps.
        source: Box<Refusal>,
    },
    /// A boot failure outside the refusal vocabulary, from the `anyhow`
    /// seams: the extension admit hooks and the wasmtime calls.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Refusal {
    /// The `error_kind` label the boot-refusal counter records; `None`
    /// goes uncounted: an untyped [`Refusal::Other`] with no recoverable
    /// class, or the wait-time [`LaunchRefusal::EventLoopGone`].
    ///
    /// Exhaustive over the arms with no wildcard: a new arm fails to
    /// compile until it names a label here, and the label-set test pins
    /// the resulting set.
    pub fn error_kind(&self) -> Option<&'static str> {
        match self {
            Self::Context { source, .. } => source.error_kind(),
            Self::Boot(refusal) => Some(boot_kind(refusal)),
            Self::Load(refusal) => Some(refusal.into()),
            Self::Launch(refusal) => launch_kind(refusal),
            Self::Capability(violation) => Some(violation.into()),
            Self::Digest(_) => Some("digest_mismatch"),
            Self::Other(err) => err.chain().find_map(untyped_kind),
        }
    }

    /// The refusal under any context frames, as a downcastable error:
    /// the typed refusal for the typed arms, the wrapped untyped error
    /// for [`Refusal::Other`].
    pub fn cause(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Self::Boot(refusal) => refusal,
            Self::Load(refusal) => refusal,
            Self::Launch(refusal) => refusal,
            Self::Capability(violation) => violation,
            Self::Digest(mismatch) => mismatch,
            Self::Context { source, .. } => source.cause(),
            Self::Other(err) => &**err,
        }
    }

    /// Wrap in a [`Refusal::Context`] frame.
    pub(crate) fn context(self, context: String) -> Self {
        Self::Context {
            context,
            source: Box::new(self),
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

/// Best-effort bridge for [`Refusal::Other`]: the extension admit hooks
/// return `anyhow::Error`, so a typed refusal an embedder's hook raises
/// arrives untyped and only a downcast can recover its class. In-tree
/// boot code returns the typed arms and never takes this path.
fn untyped_kind(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    if let Some(refusal) = cause.downcast_ref::<Refusal>() {
        return refusal.error_kind();
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
    /// Wrap the error side in a [`Refusal::Context`] frame.
    fn with_refusal_context(self, context: impl FnOnce() -> String) -> Result<T, Refusal>;
}

impl<T, E: Into<Refusal>> RefusalContext<T> for Result<T, E> {
    fn with_refusal_context(self, context: impl FnOnce() -> String) -> Result<T, Refusal> {
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

    /// The closed `error_kind` value set: an operator contract, so a
    /// change here is a deliberate diff.
    const PINNED_LABELS: &[&str] = &[
        // BootRefusal, with `manifest` split into the ParseError classes.
        "namespace_claimed",
        "manifest_not_found",
        "manifest_missing",
        "unconfigured_chain_defaulted",
        "unconfigured_chain",
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
        "unknown_component_kind",
        "missing_subscription_kind",
        "invalid_subscription",
        "invalid_chain_log_address",
        "invalid_chain_log_topic",
        "non_string_subscription_filter",
        // LoadRefusal.
        "section_unclaimed",
        "extension_namespace_claimed",
        "subscription_kind_claimed",
        "section_claimed",
        "kind_registered_twice",
        "serviceless_kind",
        "worker_kind_adapter",
        "unregistered_kind",
        "unknown_event_kind",
        "digest_unpinned",
        // LaunchRefusal, less the wait-time `event_loop_gone`, which is
        // raised after a successful boot and never counted.
        "nothing_to_run",
        "all_dead_override",
        "all_dead_configured",
        "dead_hold_subs",
        // CapabilityError.
        "undeclared",
        "unknown_wasi",
        // DigestMismatch.
        "digest_mismatch",
    ];

    /// The labels one arm's `error_kind` can yield, keyed by the arm's
    /// name from `Refusal::VARIANTS`. An arm absent here panics, so a
    /// new arm cannot ship labels the pin never saw.
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
            // Context delegates to its source; Other recovers a label
            // from another arm's table or none at all.
            "Context" | "Other" => Vec::new(),
            arm => panic!("arm {arm} has no label table; add it here and extend PINNED_LABELS"),
        }
    }

    /// Every label `error_kind` can emit, derived arm by arm over
    /// `Refusal::VARIANTS` so a new arm cannot escape the pin.
    fn derived_labels() -> Vec<&'static str> {
        Refusal::VARIANTS
            .iter()
            .flat_map(|arm| arm_labels(arm))
            .collect()
    }

    /// One representative per arm and the label it must map to, keyed
    /// like [`arm_labels`]; a new arm panics until it appears here too.
    fn representative(arm: &str) -> (Refusal, Option<&'static str>) {
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
            "Capability" => (
                CapabilityError::UnknownWasi {
                    wit_import: "wasi:sockets/tcp@0.2.0".to_owned(),
                }
                .into(),
                Some("unknown_wasi"),
            ),
            "Digest" => (
                DigestMismatch {
                    path: PathBuf::from("pinned.wasm"),
                    declared: crate::digest::ContentDigest::of_bytes(b"declared"),
                    actual: crate::digest::ContentDigest::of_bytes(b"actual"),
                }
                .into(),
                Some("digest_mismatch"),
            ),
            "Context" => (
                Refusal::from(manifest_missing()).context("load module orphan.wasm".to_owned()),
                Some("manifest_missing"),
            ),
            "Other" => (anyhow::anyhow!("engine gone").into(), None),
            arm => panic!("arm {arm} has no representative; add one"),
        }
    }

    /// The label set is closed: exactly the pinned values, one label per
    /// refusal class, none shared.
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

    /// Every arm's representative maps to its expected label, inside its
    /// own table and the pinned set; `Other` carries no label.
    #[test]
    fn every_arm_maps_into_the_pinned_set() {
        for arm in Refusal::VARIANTS {
            let (refusal, expected) = representative(arm);
            assert_eq!(refusal.error_kind(), expected, "arm {arm}: {refusal}");
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

    /// The label survives context frames, as the counter sees it.
    #[test]
    fn context_frames_keep_the_root_label() {
        let refusal = Refusal::from(BootRefusal::ManifestMissing {
            component: PathBuf::from("orphan.wasm"),
        })
        .context("load module orphan.wasm".to_owned());
        assert_eq!(refusal.error_kind(), Some("manifest_missing"));
    }

    /// The point of splitting the flat `manifest` label: each manifest
    /// refusal counts under its own ParseError class.
    #[test]
    fn a_manifest_refusal_counts_under_its_parse_class() {
        let refusal = Refusal::from(BootRefusal::Manifest(ParseError::MissingCapabilities));
        assert_eq!(refusal.error_kind(), Some("missing_capabilities"));
        let refusal = Refusal::from(BootRefusal::Manifest(ParseError::BlankModuleName));
        assert_eq!(refusal.error_kind(), Some("blank_module_name"));
    }

    /// An extension admit hook returns `anyhow::Error`, so a typed
    /// refusal it raises lands in `Other`; the label survives the wrap
    /// as it did when the counter walked the whole chain.
    #[test]
    fn a_typed_refusal_inside_an_untyped_wrap_keeps_its_label() {
        let hook_err = anyhow::Error::new(CapabilityError::UnknownWasi {
            wit_import: "wasi:sockets/tcp@0.2.0".to_owned(),
        })
        .context("install refused for module.wasm");
        assert_eq!(
            Refusal::from(hook_err).error_kind(),
            Some("unknown_wasi"),
            "the extension seam keeps the class it smuggled through anyhow",
        );
        assert_eq!(
            Refusal::from(anyhow::anyhow!("engine gone")).error_kind(),
            None,
            "an untyped failure is counted under no kind rather than a wrong one",
        );
    }
}
