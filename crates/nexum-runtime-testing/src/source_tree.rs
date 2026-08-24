//! Derived enumeration of the workspace's crate sources, for the guards that
//! read source text.

use std::path::{Path, PathBuf};

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

    #[test]
    fn the_enumeration_reaches_crates_outside_the_crates_directory() {
        let roots = crate_source_roots(&crate::workspace_root());
        let holds = |suffix: &str| roots.iter().any(|r| r.ends_with(suffix));
        assert!(holds("crates/nexum-runtime-testing/src"), "{roots:?}");
        assert!(holds("modules/example/src"), "{roots:?}");
    }

    #[test]
    #[should_panic(expected = "the walk lost its root")]
    fn an_empty_enumeration_refuses_rather_than_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate_source_roots(dir.path());
    }
}
