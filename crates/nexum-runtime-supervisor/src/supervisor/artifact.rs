//! The single production compile path. Digest verification happens on the
//! exact bytes handed to the compiler, so any refusal precedes compile and
//! the verified bytes are the compiled bytes; a guard test pins every
//! production compile call to this file.

use std::path::Path;

use anyhow::{Context, Error};
use tracing::{debug, warn};
use wasmtime::component::Component;
use wasmtime::{CodeBuilder, Engine};

use super::load::LoadRefusal;
use crate::error::{EngineRefusal, RuntimeError};
use nexum_primitives::digest::{ContentDigest, DigestMismatch, DigestPin};

/// Digest expectations for one artifact. Both pins are independent and both
/// are verified when present, so a disagreement between them refuses.
pub(super) struct DigestPolicy<'a> {
    /// The `[[modules]].digest` pin, checked first so a disagreement reports
    /// the operator's expectation.
    pub(super) operator: Option<&'a ContentDigest>,
    /// The manifest's `[component].digest` pin.
    pub(super) author: Option<&'a ContentDigest>,
    /// `[engine].require_component_digest`. An author pin does not excuse an
    /// absent operator pin (ADR-0025).
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
    // Operator first on a disagreement: a mismatch reports the operator's
    // expectation.
    if let Some(operator) = pins.operator {
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
    // A mismatch stays a typed arm of the refusal: callers match on it.
    if let Some(declared) = pins.author {
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
    if pins.operator.is_none() {
        // Every present pin is checked above before this refuses, so the
        // `actual` it tells the operator to paste is never a value a pin
        // already on disk contradicts.
        if pins.require_operator {
            return Err(LoadRefusal::DigestUnpinned {
                path: path.to_owned(),
                actual,
            }
            .into());
        }
        if pins.author.is_none() {
            warn!(
                component = %path.display(),
                digest = %actual,
                "no [[modules]].digest and no [component].digest - loading unverified",
            );
        }
    }
    let component = CodeBuilder::new(engine)
        .wasm_binary_or_text(&bytes, Some(path))
        .and_then(compile)
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("compile {}", path.display()))
        .map_err(EngineRefusal::new)?;
    Ok((component, actual))
}

/// The workspace's one exemption from the compile-constructor ban in
/// `clippy.toml`.
///
/// It is a function so the `#[allow]` covers one call and nothing else. The
/// token is the escape hatch the ban creates, so a guard test in
/// `supervisor/tests/digest.rs` refuses a second production file that carries
/// it.
#[allow(clippy::disallowed_methods)]
fn compile(builder: &mut CodeBuilder<'_>) -> Result<Component, wasmtime::Error> {
    builder.compile_component()
}
