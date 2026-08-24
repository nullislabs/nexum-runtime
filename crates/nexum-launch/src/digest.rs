//! The `digest` subcommand: hash an artifact and print its pin.

use std::io::Write;
use std::path::Path;

use nexum_runtime::config::ContentDigest;

/// Write one `sha256:<hex>` line for the artifact at `path`.
///
/// The line carries the digest and nothing else, so it pastes into a
/// `[[modules]].digest` or `[component].digest` key unedited.
pub(crate) fn write_digest(path: &Path, out: &mut impl Write) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    writeln!(out, "{}", ContentDigest::of_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NIST sha256 test vector for "abc", in the manifest grammar.
    const ABC_DIGEST: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// The whole line is the pin: a `digest = ` key takes it verbatim.
    #[test]
    fn writes_the_bare_digest_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("artifact.wasm");
        std::fs::write(&path, b"abc").expect("seed the artifact");

        let mut out = Vec::new();
        write_digest(&path, &mut out).expect("hash the artifact");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            format!("{ABC_DIGEST}\n")
        );
    }

    /// An unreadable artifact reports the io failure rather than hashing
    /// zero bytes into a digest the operator would then pin.
    #[test]
    fn a_missing_artifact_refuses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut out = Vec::new();
        let err = write_digest(&dir.path().join("absent.wasm"), &mut out)
            .expect_err("a missing file refuses");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(out.is_empty(), "nothing is printed on a refusal");
    }
}
