//! The escape hatch the `clippy.toml` bans create.
//!
//! Each ban costs a suppression, and that token is what a later author copies
//! to the site the ban exists to prevent. A lint attribute cannot suppress a
//! test, so it is counted here.

// A guard that cannot read the tree it checks has nothing to recover from.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

use nexum_runtime_guards::{crate_source_roots, workspace_root};

/// The two funnels: `read_verified_component` and the fault funnel.
///
/// Counted per occurrence, not per file: a second exemption inside a listed
/// file is the cheapest way to reopen a ban and changes no file name.
///
/// Nothing is skipped for its name; a file is production by declaration.
#[test]
fn only_the_two_funnels_suppress_a_disallowed_method() {
    let root = workspace_root();
    let mut sites = Vec::new();
    for dir in walked_dirs(&root) {
        collect_allow_sites(&root, &dir, &mut sites);
    }
    // Sorted for a stable message; `read_dir` order is filesystem-defined.
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

/// Every compiled directory: `src`, plus `tests`, `examples` and `benches`.
/// `crate_source_roots` gives only `src`, which is less than this needs.
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

/// Every crate manifest and the workspace one: a `[lints.clippy]` entry
/// disables the lint crate-wide and is not an attribute the source walk sees.
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

/// Never spelled joined to its tool prefix: the walk covers this file, and a
/// needle that spells itself counts itself.
const LINT: &str = "disallowed_methods";

/// Recurses so a nested module cannot hide the token. Whitespace is squashed,
/// so spacing inside the attribute does not change the count.
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
