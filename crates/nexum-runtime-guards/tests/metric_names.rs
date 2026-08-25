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
const TABLE_SRC: &str = "crates/nexum-runtime-metrics/src";

/// Every `GATE` that opens its own line, as that line's start and the offset
/// past the attribute. One in a doc comment or a string is not a gate.
fn gates(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(GATE) {
        let at = from + i;
        from = at + GATE.len();
        let line = text[..at].rfind('\n').map_or(0, |nl| nl + 1);
        if text[line..at].chars().all(char::is_whitespace) {
            out.push((line, from));
        }
    }
    out
}

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

/// `text` with each inline `#[cfg(test)] mod` cut out and the rest kept: a
/// file may carry shipped code after one. The cut ends at the first `}` on the
/// gate's indentation; anything unparsed stays in, which only over-scans.
fn shipped_region(text: &str) -> String {
    let mut out = String::new();
    let mut kept = 0;
    for (line, after) in gates(text) {
        if line < kept || !matches!(gated_module(&text[after..]), Some((_, false))) {
            continue;
        }
        let close = format!("\n{}}}\n", &text[line..after - GATE.len()]);
        out.push_str(&text[kept..line]);
        kept = text[after..]
            .find(&close)
            .map_or(text.len(), |end| after + end + close.len());
    }
    out.push_str(&text[kept..]);
    out
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
    gates(text)
        .into_iter()
        .filter_map(|(_, after)| match gated_module(&text[after..]) {
            Some((name, true)) => Some(name),
            _ => None,
        })
        .flat_map(|name| [dir.join(name).with_extension("rs"), dir.join(name)])
        .collect()
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
    let table_src = root.join(TABLE_SRC);
    let all = crate_source_roots(&root);
    let mut stack: Vec<PathBuf> = all
        .iter()
        .filter(|src| **src != table_src)
        .cloned()
        .collect();
    assert_eq!(
        stack.len() + 1,
        all.len(),
        "{TABLE_SRC} is not a crate source root, so this guard would scan the table itself",
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
    // The reported emit site is the first file holding the name.
    sources.sort();

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
        "described but never emitted, remove from METRICS in {TABLE_SRC}: {unused:?}",
    );
}

#[test]
fn a_name_only_a_gated_inline_module_uses_is_not_emitted() {
    let text = "fn emit() {}\n#[cfg(test)]\nmod tests {\n    \"nexum_runtime_gone_total\";\n}\n";
    assert!(!shipped_region(text).contains("nexum_runtime_gone_total"));
}

#[test]
fn code_after_a_gated_inline_module_is_still_scanned() {
    let text = "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n\"nexum_runtime_late_total\";\n";
    assert!(shipped_region(text).contains("nexum_runtime_late_total"));
}

#[test]
fn a_gate_that_does_not_open_its_line_is_not_a_gate() {
    let text = "/// `#[cfg(test)] mod x {`\n\"nexum_runtime_kept_total\";\n";
    assert!(shipped_region(text).contains("nexum_runtime_kept_total"));
    assert!(gated_module_paths(Path::new("/w/src/lib.rs"), "// #[cfg(test)] mod x;\n").is_empty());
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
