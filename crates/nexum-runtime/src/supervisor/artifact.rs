//! The single production compile path. Digest verification happens on the
//! exact bytes handed to the compiler, so any refusal precedes compile and
//! the verified bytes are the compiled bytes; a guard test pins every
//! production compile call to this file.

use std::path::Path;

use anyhow::{Context, Error, Result};
use tracing::{debug, warn};
use wasmtime::component::Component;
use wasmtime::{CodeBuilder, Engine};

use super::load::LoadRefusal;
use crate::digest::{ContentDigest, DigestMismatch};

/// The only production compile path; the verified bytes are the compiled bytes.
pub(super) fn read_verified_component(
    engine: &Engine,
    path: &Path,
    declared: Option<&ContentDigest>,
    require_digest: bool,
) -> Result<(Component, ContentDigest)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read component {}", path.display()))?;
    let actual = ContentDigest::of_bytes(&bytes);
    match declared {
        // A mismatch stays its own anyhow root: callers downcast to `DigestMismatch`.
        Some(declared) => {
            if actual != *declared {
                return Err(DigestMismatch {
                    path: path.to_owned(),
                    declared: *declared,
                    actual,
                }
                .into());
            }
            debug!(component = %path.display(), digest = %actual, "component digest verified");
        }
        None if require_digest => {
            return Err(LoadRefusal::DigestUnpinned {
                path: path.to_owned(),
            }
            .into());
        }
        None => warn!(
            component = %path.display(),
            digest = %actual,
            "no [module].component digest - loading unverified",
        ),
    }
    let component = CodeBuilder::new(engine)
        .wasm_binary_or_text(&bytes, Some(path))
        .and_then(|builder| builder.compile_component())
        // wasmtime::Error is not StdError, so anyhow's with_context needs the bridge.
        .map_err(Error::from)
        .with_context(|| format!("compile {}", path.display()))?;
    Ok((component, actual))
}
