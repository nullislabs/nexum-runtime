//! `nexum digest` over the built binary.
//!
//! The short-circuit lives in `nexum_launch::run`, which reads the process
//! arguments and owns the stream it prints to, so only a spawned process
//! shows that the subcommand answers instead of booting the engine.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The NIST sha256 test vector for "abc", in the manifest grammar.
const ABC_DIGEST: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A path under the per-target temp dir cargo hands an integration test.
fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn digest(path: &Path) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_nexum"))
        .arg("digest")
        .arg(path)
        .output()
}

/// stdout carries the pin and nothing else, and stderr stays empty: the
/// subcommand answers ahead of the config load, so neither a boot refusal
/// nor a log line can land in the value the operator pastes.
#[test]
fn digest_prints_the_pin_and_exits_zero() {
    let path = scratch("digest-hit.wasm");
    std::fs::write(&path, b"abc").expect("seed the artifact");

    let out = digest(&path).expect("spawn nexum");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8"),
        format!("{ABC_DIGEST}\n"),
    );
    assert!(out.stderr.is_empty(), "{:?}", String::from_utf8(out.stderr));
}

/// A refusal exits non-zero with an empty stdout, so `nexum digest > pin`
/// leaves no half-written pin behind, and it names the path that the io
/// error alone does not carry.
#[test]
fn a_missing_artifact_exits_non_zero() {
    let path = scratch("digest-absent.wasm");
    let _ = std::fs::remove_file(&path);

    let out = digest(&path).expect("spawn nexum");
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{:?}", String::from_utf8(out.stdout));
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(stderr.contains("cannot digest"), "{stderr}");
    assert!(stderr.contains("digest-absent.wasm"), "{stderr}");
}
