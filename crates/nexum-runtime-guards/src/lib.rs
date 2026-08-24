//! Guards on the repository rather than on a crate.
//!
//! Every guard beside this file reads the whole workspace source tree, so it
//! is a test of the repository and not of whichever crate it once sat in.
//! Holding them here keeps a guard from adding a dependency edge to a crate
//! it only polices, and the `publish = false` member costs nothing because
//! nothing depends on it.
//!
//! The library half is the enumeration the guards walk. The guards are the
//! integration tests beside it.

#![forbid(unsafe_code)]
// Guard support, so `allow-unwrap-in-tests` never sees it. A walk that cannot
// read the tree it exists to check has nothing to recover from.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

/// The workspace root: the nearest ancestor of this crate whose `Cargo.toml`
/// declares a `[workspace]`.
///
/// Nearest rather than outermost, so a checkout nested inside an unrelated
/// cargo workspace walks this tree and not the one around it.
pub fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|text| text.contains("[workspace]"))
        })
        .unwrap_or(manifest)
        .to_path_buf()
}

/// The `src` of every crate under `root`, found on disk rather than read from
/// `workspace.members`.
///
/// Cargo adopts a path dependency as a member without a table entry, so the
/// table under-enumerates. Enumerating nothing means the walk lost its root
/// rather than that the tree holds no crate, so this refuses instead of
/// handing a caller a vacuous pass.
pub fn crate_source_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    collect(root, &mut roots);
    assert!(
        !roots.is_empty(),
        "enumerated no crate sources under {}; the walk lost its root",
        root.display(),
    );
    roots
}

fn collect(dir: &Path, roots: &mut Vec<PathBuf>) {
    let src = dir.join("src");
    if dir.join("Cargo.toml").is_file() && src.is_dir() {
        roots.push(src);
    }
    for entry in std::fs::read_dir(dir).expect("read a workspace directory") {
        let path = entry.expect("directory entry").path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .expect("directory entry name")
            .to_string_lossy()
            .into_owned();
        // Build output and the dot directories host no crate of ours, and a
        // caller walks `src` itself.
        if name == "src" || name == "target" || name.starts_with('.') {
            continue;
        }
        collect(&path, roots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the distance, not the shape: any ancestor carrying a
    /// `[workspace]` satisfies the search, so only the depth says the walk
    /// starts at this repository.
    #[test]
    fn the_root_is_the_workspace_and_not_this_crate() {
        assert_eq!(
            workspace_root().join("crates").join("nexum-runtime-guards"),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        );
    }

    #[test]
    fn the_enumeration_reaches_crates_outside_the_crates_directory() {
        let roots = crate_source_roots(&workspace_root());
        let holds = |suffix: &str| roots.iter().any(|root| root.ends_with(suffix));
        assert!(holds("crates/nexum-runtime-metrics/src"), "{roots:?}");
        assert!(holds("modules/example/src"), "{roots:?}");
    }

    #[test]
    #[should_panic(expected = "the walk lost its root")]
    fn an_empty_enumeration_refuses_rather_than_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate_source_roots(dir.path());
    }
}
