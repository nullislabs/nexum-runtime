//! The escape hatch the `clippy.toml` bans create.
//!
//! Each ban costs a suppression, and that token is what a later author copies
//! to the site the ban exists to prevent. A lint attribute cannot suppress a
//! test, so it is counted here. A blanket allow carries no such token, so it
//! is refused outright rather than enumerated (#353).

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
    let suppression = format!("clippy::{LINT}");
    let mut sites = Vec::new();
    for path in walked_files(&root) {
        let count = squash(&read(&path)).matches(&suppression).count();
        if count > 0 {
            sites.push((relative(&root, &path), count));
        }
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
        assert!(
            !read(&manifest).contains(LINT),
            "{} sets the ban's level for a whole crate, which no attribute in \
             the walk above would show; clippy.toml owns the ban",
            manifest.display(),
        );
    }
}

#[test]
fn no_blanket_suppression_reopens_a_ban() {
    let root = workspace_root();
    let mut found = Vec::new();
    for path in walked_files(&root) {
        for needle in blanket_suppressions(&squash(&read(&path))) {
            found.push(format!("{}: {needle}", relative(&root, &path)));
        }
    }
    for manifest in manifests(&root) {
        if let Some(key) = blanket_manifest_key(&read(&manifest)) {
            found.push(format!("{}: lints entry {key}", relative(&root, &manifest)));
        }
    }
    found.sort();
    assert!(
        found.is_empty(),
        "a blanket suppression lifts every ban in clippy.toml while naming \
         none; name the one lint at the one site instead: {found:?}",
    );
}

/// Every compiled `.rs` file: under `src`, plus `tests`, `examples` and
/// `benches`. `crate_source_roots` gives only `src`, which is less than this
/// needs.
fn walked_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for src in crate_source_roots(root) {
        let crate_dir = crate_dir(&src);
        files.extend(rust_files(&src));
        files.extend(
            ["tests", "examples", "benches"]
                .into_iter()
                .map(|target| crate_dir.join(target))
                .filter(|dir| dir.is_dir())
                .flat_map(|dir| rust_files(&dir)),
        );
    }
    files
}

/// Recurses so a nested module cannot hide a token.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read a crate source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}

/// Every crate manifest and the workspace one: a `[lints]` entry sets a level
/// crate-wide and is not an attribute the source walk sees.
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read a walked file")
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("a walked file lives under the workspace root")
        .to_string_lossy()
        .into_owned()
}

/// Squashed, so spacing inside an attribute changes no match.
fn squash(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Never spelled joined to its tool prefix: the walk covers this file, and a
/// needle that spells itself counts itself.
const LINT: &str = "disallowed_methods";

/// Every clippy lint group, not the subset covering today's bans:
/// `restriction` holds the workspace's `unwrap_used`. Same rule as `LINT`:
/// joined to its tool prefix at use, never in source.
const BLANKET_GROUPS: [&str; 10] = [
    "all",
    "cargo",
    "complexity",
    "correctness",
    "nursery",
    "pedantic",
    "perf",
    "restriction",
    "style",
    "suspicious",
];
const BLANKET_WARNINGS: &str = "warnings";

fn blanket_needles() -> Vec<String> {
    let mut needles: Vec<String> = BLANKET_GROUPS
        .iter()
        .map(|group| format!("clippy::{group}"))
        .collect();
    needles.push(BLANKET_WARNINGS.to_owned());
    needles
}

/// Each blanket needle in `squashed` that sits in an `allow` or `expect`
/// argument list, whatever attribute wraps it. Delimited either side, since
/// `all` is a prefix of `allow_attributes` and prose is not an attribute.
fn blanket_suppressions(squashed: &str) -> Vec<String> {
    let mut found = Vec::new();
    for needle in blanket_needles() {
        for (at, _) in squashed.match_indices(&needle) {
            let before = squashed[..at].chars().next_back();
            let after = squashed[at + needle.len()..].chars().next();
            if !matches!(before, Some('(' | ',')) || !matches!(after, Some(')' | ',')) {
                continue;
            }
            if matches!(enclosing_call(&squashed[..at]), Some("allow" | "expect")) {
                found.push(needle.clone());
            }
        }
    }
    found
}

/// The name whose argument list is still open at the end of `prefix`.
fn enclosing_call(prefix: &str) -> Option<&str> {
    let bytes = prefix.as_bytes();
    let mut depth = 0usize;
    let mut at = bytes.len();
    while at > 0 {
        at -= 1;
        match bytes[at] {
            b')' => depth += 1,
            b'(' if depth == 0 => {
                // Byte-wise: a trailing byte of a multi-byte char ends the
                // name, so `boundary + 1` is a char boundary and never splits.
                let start = bytes[..at]
                    .iter()
                    .rposition(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
                    .map_or(0, |boundary| boundary + 1);
                return if start == at {
                    None
                } else {
                    Some(&prefix[start..at])
                };
            }
            b'(' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// A `[lints]` key naming a whole group and lowering its level.
///
/// Line-wise, so a commented-out entry keeps its `#` and misses the key.
fn blanket_manifest_key(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let (key, level) = line.split_once('=')?;
        let key = key.trim();
        let named = BLANKET_GROUPS.contains(&key) || key == BLANKET_WARNINGS;
        (named && level.contains("allow")).then_some(key)
    })
}

mod parsing {
    use super::*;

    #[test]
    fn every_suppressing_attribute_form_is_seen() {
        let needle = &blanket_needles()[0];
        for form in [
            "#![allow(NEEDLE)]",
            "#[allow(NEEDLE)]",
            "#[expect(NEEDLE, reason = \"\")]",
            "#[cfg_attr(test, allow(NEEDLE))]",
            "#[allow(dead_code, NEEDLE)]",
        ] {
            let source = squash(&form.replace("NEEDLE", needle));
            assert_eq!(
                blanket_suppressions(&source),
                std::slice::from_ref(needle),
                "{form}"
            );
        }
    }

    #[test]
    fn every_blanket_spelling_is_a_needle() {
        for needle in blanket_needles() {
            let source = squash(&format!("#[allow({needle})]"));
            assert_eq!(
                blanket_suppressions(&source),
                std::slice::from_ref(&needle),
                "{needle}"
            );
        }
    }

    #[test]
    fn a_narrow_lint_a_denial_and_prose_are_not_suppressions() {
        for source in [
            "#[allow(clippy::allow_attributes)]",
            "#[allow(clippy::too_many_arguments)]",
            "#[deny(warnings)]",
            "//! CI runs with warnings denied.",
            "fn no_warnings() {}",
            // Non-ASCII before the paren: the name scan must not split a char.
            "//! Café(warnings)",
        ] {
            assert!(blanket_suppressions(&squash(source)).is_empty(), "{source}",);
        }
    }

    #[test]
    fn a_group_entry_in_a_lints_table_is_a_suppression() {
        for group in BLANKET_GROUPS.into_iter().chain([BLANKET_WARNINGS]) {
            let table = |level: &str| format!("[lints.clippy]\n{group} = {level}\n");
            assert_eq!(blanket_manifest_key(&table("\"allow\"")), Some(group));
            assert_eq!(
                blanket_manifest_key(&table("{ level = \"allow\", priority = -1 }")),
                Some(group),
            );
            assert_eq!(blanket_manifest_key(&table("\"deny\"")), None, "{group}");
            assert_eq!(
                blanket_manifest_key(&format!("# {group} = \"allow\"\n")),
                None,
                "{group}",
            );
        }
    }
}
