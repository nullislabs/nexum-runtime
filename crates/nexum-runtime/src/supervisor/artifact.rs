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
use crate::digest::{ContentDigest, DigestMismatch, DigestPin};
use crate::refusal::Refusal;

/// Digest expectations for one artifact. The operator's `[implements]`
/// pin and the author's `[component].digest` are independent: both are
/// verified when present, so a disagreement between them refuses.
pub(super) struct DigestPolicy<'a> {
    /// The `[implements]` row's pin; checked first, so a disagreement
    /// reports the operator's expectation.
    pub(super) operator: Option<&'a ContentDigest>,
    /// The manifest's `[component].digest` pin.
    pub(super) author: Option<&'a ContentDigest>,
    /// `[engine].require_component_digest`: an operator pin does not
    /// excuse a missing author pin.
    pub(super) require_author: bool,
}

#[cfg(test)]
impl<'a> DigestPolicy<'a> {
    /// The author-pin-only policy, for tests without an `[implements]` row.
    pub(super) fn author(declared: Option<&'a ContentDigest>, require: bool) -> Self {
        Self {
            operator: None,
            author: declared,
            require_author: require,
        }
    }
}

/// The only production compile path; the verified bytes are the compiled bytes.
pub(super) fn read_verified_component(
    engine: &Engine,
    path: &Path,
    pins: DigestPolicy<'_>,
) -> Result<(Component, ContentDigest), Refusal> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read component {}", path.display()))?;
    let actual = ContentDigest::of_bytes(&bytes);
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
        debug!(component = %path.display(), digest = %actual, "operator [implements] pin verified");
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
        None if pins.require_author => {
            return Err(LoadRefusal::DigestUnpinned {
                path: path.to_owned(),
            }
            .into());
        }
        None if pins.operator.is_none() => warn!(
            component = %path.display(),
            digest = %actual,
            "no [component].digest digest - loading unverified",
        ),
        None => {}
    }
    let component = CodeBuilder::new(engine)
        .wasm_binary_or_text(&bytes, Some(path))
        .and_then(|builder| builder.compile_component())
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("compile {}", path.display()))?;
    Ok((component, actual))
}
