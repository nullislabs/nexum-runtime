//! Every emitted metric name is described, and every described name is emitted.
//!
//! A name reaching `/metrics` with no `METRICS` entry carries no HELP or TYPE,
//! and an entry nothing emits promises a series that never appears.

// A guard that cannot read the tree it checks has nothing to recover from.
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexum_runtime_guards::{crate_source_roots, workspace_root};
use nexum_runtime_metrics::METRICS;

const GATE: &str = "#[cfg(test)]";

/// The module `GATE` gates, and whether it is a separate file.
fn gated_module(after_gate: &str) -> Option<(&str, bool)> {
    let mut head = after_gate.trim_start();
    for vis in ["pub(crate)", "pub(super)", "pub"] {
        if let Some(tail) = head.strip_prefix(vis) {
            head = tail.trim_start();
            break;
        }
    }
    let rest = head.strip_prefix("mod ")?;
    let end = rest.find([';', '{'])?;
    Some((rest[..end].trim(), rest.as_bytes()[end] == b';'))
}

/// Text before the first inline `#[cfg(test)] mod`. A gated item that is not a
/// module is kept, which only over-scans.
///
/// Widens `shipped_region` in `nexum-runtime-wasm`, which also cuts at the
/// declaration form: that would drop the rest of a file whose gated module is
/// declared near the top, so a declaration is excluded by path instead.
fn shipped_region(text: &str) -> &str {
    let mut from = 0;
    while let Some(i) = text[from..].find(GATE) {
        let at = from + i;
        if let Some((_, false)) = gated_module(&text[at + GATE.len()..]) {
            return &text[..at];
        }
        from = at + GATE.len();
    }
    text
}

/// Prefixes of the paths a `#[cfg(test)] mod NAME;` in `file` gates, both
/// spellings. `shipped_region` cannot see them: the gated code is a sibling.
fn gated_module_paths(file: &Path, text: &str) -> Vec<PathBuf> {
    let dir = if file
        .file_stem()
        .is_some_and(|stem| stem == "mod" || stem == "lib" || stem == "main")
    {
        file.parent()
            .expect("a source file has a parent")
            .to_owned()
    } else {
        file.with_extension("")
    };
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(GATE) {
        let at = from + i;
        if let Some((name, true)) = gated_module(&text[at + GATE.len()..]) {
            out.push(dir.join(name).with_extension("rs"));
            out.push(dir.join(name));
        }
        from = at + GATE.len();
    }
    out
}

/// Derived, so a crate that starts emitting is in the walk at once.
///
/// The table's own crate is excluded: scanning it would find every name in the
/// table's literals and the unused half would pass over nothing. Removing
/// exactly one root is asserted, so a rename fails here rather than quietly
/// making that half vacuous.
#[test]
fn every_emitted_name_is_in_the_table_and_every_entry_is_emitted() {
    let root = workspace_root();
    let table_src = root.join("crates/nexum-runtime-metrics/src");
    let all = crate_source_roots(&root);
    let mut stack: Vec<PathBuf> = all
        .iter()
        .filter(|src| **src != table_src)
        .cloned()
        .collect();
    assert_eq!(
        stack.len() + 1,
        all.len(),
        "{} is not a crate source root, so this guard would scan the table itself",
        table_src.display(),
    );

    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the crate source tree") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            sources.push((path, text));
        }
    }

    let gated: Vec<PathBuf> = sources
        .iter()
        .flat_map(|(path, text)| gated_module_paths(path, text))
        .collect();

    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (path, text) in &sources {
        if gated.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        let shipped = shipped_region(text);
        for (idx, _) in shipped.match_indices("\"nexum_runtime_") {
            let rest = &shipped[idx + 1..];
            if let Some(end) = rest.find('"') {
                found
                    .entry(rest[..end].to_owned())
                    .or_insert_with(|| path.clone());
            }
        }
    }

    let table: BTreeSet<&str> = METRICS.iter().map(|metric| metric.name).collect();
    let missing: Vec<String> = found
        .iter()
        .filter(|(name, _)| !table.contains(name.as_str()))
        .map(|(name, path)| {
            let at = path.strip_prefix(&root).unwrap_or(path);
            format!("{name} in {}", at.display())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "emitted but undescribed, add to METRICS: {missing:?}",
    );

    let unused: Vec<&&str> = table
        .iter()
        .filter(|name| !found.contains_key(**name))
        .collect();
    assert!(
        unused.is_empty(),
        "described but never emitted, remove from METRICS in {}: {unused:?}",
        table_src.display(),
    );
}

#[test]
fn a_name_only_a_gated_inline_module_uses_is_not_emitted() {
    let text = "fn emit() {}\n#[cfg(test)]\nmod tests {\n    \"nexum_runtime_gone_total\";\n}\n";
    assert!(!shipped_region(text).contains("nexum_runtime_gone_total"));
}

#[test]
fn a_gated_module_declaration_does_not_truncate_the_file() {
    let text = "#[cfg(test)]\nmod test_support;\n\"nexum_runtime_kept_total\";\n";
    assert!(shipped_region(text).contains("nexum_runtime_kept_total"));
}

#[test]
fn a_gated_module_declaration_hides_its_file_and_its_subtree() {
    let gated = gated_module_paths(
        Path::new("/w/src/supervisor/mod.rs"),
        "#[cfg(test)]\npub(crate) mod tests;\n",
    );
    let hidden = |path: &str| {
        gated
            .iter()
            .any(|prefix| Path::new(path).starts_with(prefix))
    };
    assert!(hidden("/w/src/supervisor/tests.rs"), "{gated:?}");
    assert!(hidden("/w/src/supervisor/tests/dispatch.rs"), "{gated:?}");
    assert!(!hidden("/w/src/supervisor/dispatch.rs"), "{gated:?}");
}

#[test]
fn an_ungated_module_declaration_stays_in_the_walk() {
    let gated = gated_module_paths(Path::new("/w/src/lib.rs"), "mod dispatch;\n");
    assert!(gated.is_empty(), "{gated:?}");
}
