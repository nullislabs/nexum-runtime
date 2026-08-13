//! The component vocabulary is service (ADR-0016); `provider` is reserved
//! for the alloy chain RPC sense. Two scans hold the line from both sides:
//! the component path must not regrow `provider`, and the alloy seam must
//! keep it.

use std::fs;
use std::path::{Path, PathBuf};

/// Files whose `provider` vocabulary is wholly alloy-owned; exempt from the
/// negative scan, anchored by [`ALLOY_ANCHORS`].
const ALLOY_SEAM_FILES: &[&str] = &["host/provider_pool.rs", "test_utils/rpc.rs"];

/// A line in any other file may spell `provider` only beside one of these
/// alloy identifiers (compared lowercase). Prose phrases are deliberately
/// absent: a marker must be a spelling no component-path line would carry.
const ALLOY_MARKERS: &[&str] = &[
    "alloy",
    "providerpool",
    "provider_pool",
    "provider pool",
    "dynprovider",
    "providerbuilder",
    "pool.provider(",
];

/// Exact lines (trimmed) that are alloy-sense but carry no marker of their
/// own, e.g. a binding of a `pool.provider(..)` result. Add a line here for
/// a genuine chain RPC use the markers cannot reach; never for prose.
const ALLOY_LINES: &[&str] = &[
    "Ok(provider) => provider,",
    "\"chain-log provider lookup failed - retrying after backoff\",",
    "let head = match provider.get_block_number().await {",
    "match provider.get_block_by_number(t.number.into()).await {",
    "(Chain::mainnet(), block_node.provider()),",
    "(Chain::from_id(100), log_node.provider()),",
];

/// Spellings each file must keep, so a blanket `provider` to `service`
/// substitution fails here rather than at the build.
const ALLOY_ANCHORS: &[(&str, &[&str])] = &[
    (
        "host/provider_pool.rs",
        &["pub struct ProviderPool", "DynProvider", "pub fn provider("],
    ),
    ("test_utils/rpc.rs", &["DynProvider", "pub fn provider("]),
    ("runtime/event_loop.rs", &["pool.provider("]),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rs_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[test]
fn the_component_path_spells_service_not_provider() {
    let src = src_dir();
    let mut files = Vec::new();
    rs_files(&src, &mut files).expect("walk src");
    assert!(!files.is_empty(), "the src scan found no Rust files");

    let mut offences = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&src)
            .expect("path under src")
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOY_SEAM_FILES.contains(&relative.as_str()) {
            continue;
        }
        let contents = fs::read_to_string(&path).expect("read source file");
        for (idx, line) in contents.lines().enumerate() {
            let lower = line.to_lowercase();
            if !lower.contains("provider") {
                continue;
            }
            if ALLOY_MARKERS.iter().any(|m| lower.contains(m)) {
                continue;
            }
            if ALLOY_LINES.contains(&line.trim()) {
                continue;
            }
            offences.push(format!("{relative}:{}: {}", idx + 1, line.trim()));
        }
    }

    assert!(
        offences.is_empty(),
        "`provider` spellings without an alloy marker; the component \
         concept is spelled service (see issue #202 and ADR-0016). Either \
         rename to service, or, for a genuine chain RPC use, keep an alloy \
         identifier on the line or add the exact line to ALLOY_LINES:\n{}",
        offences.join("\n"),
    );
}

#[test]
fn the_alloy_seam_keeps_its_provider_spelling() {
    let src = src_dir();
    for (relative, anchors) in ALLOY_ANCHORS {
        let contents = fs::read_to_string(src.join(relative)).expect("read anchored file");
        for anchor in *anchors {
            assert!(
                contents.contains(anchor),
                "{relative} no longer spells `{anchor}`; the alloy chain RPC \
                 sense keeps the word provider (see issue #202 and ADR-0016), \
                 so a blanket rename must be reverted, not accommodated here",
            );
        }
    }
}
