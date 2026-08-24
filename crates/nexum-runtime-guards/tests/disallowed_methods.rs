//! The escape hatch the `clippy.toml` bans create.
//!
//! `clippy.toml` bans the wasmtime routes to a `Component` and the
//! string-carrying guest fault constructors, both by resolved path, which
//! catches an aliased import or a re-export that a source scan cannot. Each
//! ban costs a suppression, and that token is what a later author copies to
//! the site the ban exists to prevent. A lint attribute cannot suppress a
//! test, so the token is counted here instead.

// An integration-test helper sits outside a `#[test]` function, which is
// what `allow-expect-in-tests` keys on. A guard that cannot read the tree
// it exists to check has nothing to recover from.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

use nexum_runtime_guards::{crate_source_roots, workspace_root};

/// The two funnels: `read_verified_component`, which keeps the verified bytes
/// the compiled bytes, and the fault funnel, which keeps a runtime string off
/// the guest boundary.
///
/// Counted per occurrence and not per file, because a second exemption inside
/// a file already on the list is the cheapest way to reopen a ban: one more
/// `compile` helper beside the funnel in `artifact.rs` compiles bytes nothing
/// verified and changes no file name.
///
/// The walk covers `src`, `tests`, `examples` and `benches` in every crate:
/// the compile path is reachable from `nexum-runtime-wasm` and `nexum-runtime`
/// as well, and a file is production by declaration rather than by looking
/// test-shaped, so nothing is skipped for its name.
#[test]
fn only_the_two_funnels_suppress_a_disallowed_method() {
    let root = workspace_root();
    let mut sites = Vec::new();
    for dir in walked_dirs(&root) {
        collect_allow_sites(&root, &dir, &mut sites);
    }
    // Sorted so a further site fails with a stable message; `read_dir` order
    // is filesystem-defined.
    sites.sort();
    assert_eq!(
        sites,
        [
            (
                "crates/nexum-runtime-supervisor/src/supervisor/artifact.rs".to_owned(),
                1
            ),
            ("crates/nexum-runtime-wasm/src/error.rs".to_owned(), 6),
        ],
        "only read_verified_component and the fault funnel may reopen a ban \
         in clippy.toml; every other caller goes through them, and neither \
         funnel grows an exemption without this count moving",
    );

    for manifest in manifests(&root) {
        let text = std::fs::read_to_string(&manifest).expect("read a crate manifest");
        assert!(
            !text.contains(LINT),
            "{} sets the ban's level for a whole crate, which no attribute in \
             the walk above would show; clippy.toml owns the ban",
            manifest.display(),
        );
    }
}

/// Every compiled directory of every crate: `src`, plus the `tests`,
/// `examples` and `benches` targets beside it. `crate_source_roots`
/// enumerates only `src`, which is what the metric-name guard wants and less
/// than this one does.
fn walked_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for src in crate_source_roots(root) {
        let crate_dir = crate_dir(&src);
        dirs.push(src);
        dirs.extend(
            ["tests", "examples", "benches"]
                .into_iter()
                .map(|target| crate_dir.join(target))
                .filter(|dir| dir.is_dir()),
        );
    }
    dirs
}

/// The manifest of every crate, plus the workspace manifest above them: a
/// `[lints.clippy]` entry turns the lint off for a whole crate and is not an
/// attribute, so the source walk cannot see it.
fn manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.join("Cargo.toml")];
    out.extend(
        crate_source_roots(root)
            .iter()
            .map(|src| crate_dir(src).join("Cargo.toml")),
    );
    out
}

fn crate_dir(src: &Path) -> PathBuf {
    src.parent().expect("a crate src has a parent").to_owned()
}

/// The lint each ban hands to its funnel. Never spelled joined to its tool
/// prefix: the walk covers this file too, and a needle that spells itself
/// counts itself.
const LINT: &str = "disallowed_methods";

/// Recurses so a nested module cannot hide the token. Whitespace is squashed
/// first, so spacing inside the attribute does not change the count. Sites
/// are workspace-relative, since a bare file name does not say which crate it
/// came from.
fn collect_allow_sites(root: &Path, dir: &Path, sites: &mut Vec<(String, usize)>) {
    let suppression = format!("clippy::{LINT}");
    for entry in std::fs::read_dir(dir).expect("read a crate source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_allow_sites(root, &path, sites);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read a crate source file");
        let squashed: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let count = squashed.matches(&suppression).count();
        if count > 0 {
            sites.push((
                path.strip_prefix(root)
                    .expect("a walked file lives under the workspace root")
                    .to_string_lossy()
                    .into_owned(),
                count,
            ));
        }
    }
}
