//! Every emitted metric name is described, and every described name is
//! emitted.
//!
//! A metric name is an operator contract, so a name that reaches `/metrics`
//! without passing through `nexum_runtime_metrics::METRICS` carries no HELP
//! or TYPE text, and a table entry nothing emits promises a series that never
//! appears.

// An integration-test helper sits outside a `#[test]` function, which is
// what `allow-expect-in-tests` keys on. A guard that cannot read the tree
// it exists to check has nothing to recover from.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use nexum_runtime_guards::{crate_source_roots, workspace_root};
use nexum_runtime_metrics::METRICS;

/// The roots are derived, so a crate that starts emitting is in the walk the
/// moment it exists.
///
/// The crate that declares the table is the one exclusion: scanning it would
/// find every name through the table's own literals, and the unused half
/// would pass over nothing. Dropping it is asserted to remove exactly one
/// root, so a move or a rename fails here rather than quietly making that
/// half vacuous.
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

    let mut found: BTreeSet<String> = BTreeSet::new();
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
            let src = std::fs::read_to_string(&path).expect("read a source file");
            for (idx, _) in src.match_indices("\"nexum_runtime_") {
                let rest = &src[idx + 1..];
                if let Some(end) = rest.find('"') {
                    found.insert(rest[..end].to_owned());
                }
            }
        }
    }

    let table: BTreeSet<&str> = METRICS.iter().map(|metric| metric.name).collect();
    let missing: Vec<&String> = found
        .iter()
        .filter(|name| !table.contains(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "emitted but undescribed, add to METRICS: {missing:?}",
    );

    let unused: Vec<&&str> = table
        .iter()
        .filter(|name| !found.contains(**name))
        .collect();
    assert!(
        unused.is_empty(),
        "described but never emitted, remove from METRICS: {unused:?}",
    );
}
