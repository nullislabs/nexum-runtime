//! The single production compile path. Digest verification happens on the
//! exact bytes handed to the compiler, so any refusal precedes compile and
//! the verified bytes are the compiled bytes; a guard test pins every
//! production compile call to this file.
//!
//! The operator's `[[modules]].digest` is the pin the engine requires by
//! default; the author's `[component].digest` is verified when present and
//! never substitutes for it (ADR-0025).

use std::path::Path;

use anyhow::{Context, Error};
use tracing::{debug, warn};
use wasmtime::component::Component;
use wasmtime::{CodeBuilder, Engine};

use super::load::LoadRefusal;
use crate::error::{EngineRefusal, RuntimeError};
use nexum_primitives::digest::{ContentDigest, DigestMismatch, DigestPin};

/// Digest expectations for one artifact. The operator's `[[modules]]`
/// pin and the author's `[component].digest` are independent: both are
/// verified when present, so a disagreement between them refuses.
pub(super) struct DigestPolicy<'a> {
    /// The `[[modules]].digest` pin; checked first, so a disagreement
    /// reports the operator's expectation.
    pub(super) operator: Option<&'a ContentDigest>,
    /// The manifest's `[component].digest` pin.
    pub(super) author: Option<&'a ContentDigest>,
    /// `[engine].require_component_digest`: an absent operator pin
    /// refuses, and an author pin does not excuse it, because the party
    /// who can rewrite the artifact can rewrite the manifest beside it
    /// (ADR-0001, ADR-0025).
    pub(super) require_operator: bool,
}

#[cfg(test)]
impl<'a> DigestPolicy<'a> {
    /// Only the author pin, with the operator requirement relaxed.
    pub(super) fn author(declared: Option<&'a ContentDigest>) -> Self {
        Self {
            operator: None,
            author: declared,
            require_operator: false,
        }
    }
}

/// The only production compile path; the verified bytes are the compiled bytes.
pub(super) fn read_verified_component(
    engine: &Engine,
    path: &Path,
    pins: DigestPolicy<'_>,
) -> Result<(Component, ContentDigest), RuntimeError> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read component {}", path.display()))
        .map_err(EngineRefusal::new)?;
    let actual = ContentDigest::of_bytes(&bytes);
    match pins.operator {
        Some(operator) => {
            if actual != *operator {
                return Err(DigestMismatch {
                    path: path.to_owned(),
                    pin: DigestPin::Operator,
                    declared: *operator,
                    actual,
                }
                .into());
            }
            debug!(component = %path.display(), digest = %actual, "operator [[modules]].digest pin verified");
        }
        // The refusal carries `actual`, so the value it demands is
        // readable without a second run or a second tool.
        None if pins.require_operator => {
            return Err(LoadRefusal::DigestUnpinned {
                path: path.to_owned(),
                actual,
            }
            .into());
        }
        None => {}
    }
    match pins.author {
        // A mismatch stays a typed arm of the refusal: callers match on it.
        Some(declared) => {
            if actual != *declared {
                return Err(DigestMismatch {
                    path: path.to_owned(),
                    pin: DigestPin::Author,
                    declared: *declared,
                    actual,
                }
                .into());
            }
            debug!(component = %path.display(), digest = %actual, "component digest verified");
        }
        None if pins.operator.is_none() => warn!(
            component = %path.display(),
            digest = %actual,
            "no [[modules]].digest and no [component].digest - loading unverified",
        ),
        None => {}
    }
    let component = CodeBuilder::new(engine)
        .wasm_binary_or_text(&bytes, Some(path))
        .and_then(|builder| builder.compile_component())
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("compile {}", path.display()))
        .map_err(EngineRefusal::new)?;
    Ok((component, actual))
}
