//! The one refusal value the boot path returns.
//!
//! [`Refusal`] composes the typed refusal vocabulary from the supervisor,
//! the launcher, and the manifest layer, so the boot-refusal counter
//! matches on a value instead of downcasting an `anyhow` chain. The
//! `error_kind` metric label set is closed: [`Refusal::error_kind`] is
//! exhaustive over the arms, and the test below pins the resulting set.

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
    /// A boot failure outside the refusal vocabulary; never counted.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Refusal {
    /// The `error_kind` label the boot-refusal counter records, `None`
    /// for [`Refusal::Other`].
    ///
    /// Exhaustive over the arms with no wildcard: a new arm fails to
    /// compile until it names a label here, and the label-set test pins
    /// the resulting set.
    pub fn error_kind(&self) -> Option<&'static str> {
        match self {
            Self::Context { source, .. } => source.error_kind(),
            // The wrapped `ParseError` names the manifest refusal class;
            // the flat `manifest` label would collapse them into one.
            Self::Boot(BootRefusal::Manifest(parse)) => Some(parse.into()),
            Self::Boot(refusal) => Some(refusal.into()),
            Self::Load(refusal) => Some(refusal.into()),
            Self::Launch(refusal) => Some(refusal.into()),
            Self::Capability(violation) => Some(violation.into()),
            Self::Digest(_) => Some("digest_mismatch"),
            Self::Other(_) => None,
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
        // LaunchRefusal.
        "event_loop_gone",
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

    /// Every label `error_kind` can emit, derived from the same variant
    /// tables the labels come from.
    fn derived_labels() -> Vec<&'static str> {
        let mut labels: Vec<&'static str> = Vec::new();
        labels.extend(
            BootRefusal::VARIANTS
                .iter()
                .copied()
                // `error_kind` maps the Manifest arm to the wrapped
                // ParseError class, never to the flat `manifest` label.
                .filter(|v| *v != "manifest"),
        );
        labels.extend(ParseError::VARIANTS);
        labels.extend(LoadRefusal::VARIANTS);
        labels.extend(LaunchRefusal::VARIANTS);
        labels.extend(CapabilityError::VARIANTS);
        labels.push("digest_mismatch");
        labels
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

    /// One representative per arm lands inside the pinned set; `Other`
    /// carries no label.
    #[test]
    fn every_arm_maps_into_the_pinned_set() {
        let cases: Vec<(Refusal, &str)> = vec![
            (
                BootRefusal::ManifestMissing {
                    component: PathBuf::from("orphan.wasm"),
                }
                .into(),
                "manifest_missing",
            ),
            (
                LoadRefusal::SectionClaimed { section: "venue" }.into(),
                "section_claimed",
            ),
            (LaunchRefusal::EventLoopGone.into(), "event_loop_gone"),
            (
                CapabilityError::UnknownWasi {
                    wit_import: "wasi:sockets/tcp@0.2.0".to_owned(),
                }
                .into(),
                "unknown_wasi",
            ),
            (
                DigestMismatch {
                    path: PathBuf::from("pinned.wasm"),
                    declared: crate::digest::ContentDigest::of_bytes(b"declared"),
                    actual: crate::digest::ContentDigest::of_bytes(b"actual"),
                }
                .into(),
                "digest_mismatch",
            ),
        ];
        for (refusal, kind) in cases {
            assert_eq!(refusal.error_kind(), Some(kind), "{refusal}");
            assert!(PINNED_LABELS.contains(&kind), "{kind} is not pinned");
        }
        assert_eq!(
            Refusal::from(anyhow::anyhow!("engine gone")).error_kind(),
            None,
            "an untyped failure is counted under no kind rather than a wrong one",
        );
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
}
