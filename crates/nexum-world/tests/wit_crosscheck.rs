//! Cross-checks tying the [`nexum_world::CORE`] capability table to the
//! two surfaces it claims to describe: the `wit/nexum-host/` package and
//! the SDK's `bind_host_via_wit_bindgen!` macro.
//!
//! The table carries WIT import ids and bind-macro idents as string
//! literals that nothing else resolves at host-build time, so a rename
//! on either far side would otherwise surface only in a guest build.
//! These tests parse the WIT with the same `wit-parser` version
//! wasmtime v46 embeds, so the check agrees with the parser the runtime
//! actually runs.

use std::collections::BTreeSet;
use std::path::Path;

use nexum_world::CORE;

/// The repository's `wit/nexum-host` package directory, resolved from
/// this crate's manifest directory so the test is cwd-independent.
fn nexum_host_wit_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit/nexum-host")
}

/// The SDK macro source carrying the bind list and the per-capability
/// arms, read as text: the idents are `macro_rules!` matcher literals,
/// so text is the only host-side surface they have.
fn sdk_macro_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nexum-sdk/src/wit_bindgen_macro.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Every `import` literal in `CORE` resolves to a named interface in
/// the parsed `nexum:host` package, and every import-bearing row was
/// actually checked. A WIT interface rename now fails here, in a host
/// test, rather than in the next guest build.
#[test]
fn core_imports_resolve_against_the_wit_tree() {
    let mut resolve = wit_parser::Resolve::new();
    let dir = nexum_host_wit_dir();
    resolve
        .push_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot parse {}: {e:?}", dir.display()));

    let interfaces: BTreeSet<String> = resolve
        .interfaces
        .iter()
        .filter_map(|(id, _)| resolve.id_of(id))
        .collect();
    assert!(
        !interfaces.is_empty(),
        "parsing {} yielded no named interfaces",
        dir.display()
    );

    let mut checked = 0;
    for cap in CORE {
        let Some(import) = cap.import else {
            continue;
        };
        assert!(
            interfaces.contains(import),
            "capability `{}` imports `{import}`, which is not an interface in {}; \
             parsed interfaces: {interfaces:?}",
            cap.name.as_str(),
            dir.display(),
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no CORE row carries an import; test is vacuous"
    );
    assert_eq!(
        checked,
        nexum_world::CORE_IFACES.len(),
        "every import-bearing CORE row must be resolved against the WIT tree",
    );
}

/// The `adapter` idents in `CORE` are exactly the arms
/// `bind_host_via_wit_bindgen!` accepts, and exactly the blanket list
/// its zero-argument form expands to. A capability with no matching
/// bind arm, or an arm no capability reaches, fails here.
#[test]
fn core_adapters_equal_the_sdk_bind_macro_arms() {
    let source = sdk_macro_source();

    let core: BTreeSet<&str> = CORE.iter().filter_map(|cap| cap.adapter).collect();
    let arms = macro_arm_idents(&source);
    let blanket = blanket_caps_list(&source);

    assert_eq!(
        core, arms,
        "CORE adapter idents differ from the __bind_host_cap_via_wit_bindgen! arms",
    );
    assert_eq!(
        core,
        blanket.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        "CORE adapter idents differ from the blanket-form caps list",
    );
}

/// The idents of the `(ident) => {` arms inside
/// `__bind_host_cap_via_wit_bindgen!`, the per-capability dispatch the
/// bind macro accepts.
fn macro_arm_idents(source: &str) -> BTreeSet<&str> {
    let mut arms = BTreeSet::new();
    let mut in_macro = false;
    for line in source.lines() {
        if line.starts_with("macro_rules! __bind_host_cap_via_wit_bindgen") {
            in_macro = true;
            continue;
        }
        if !in_macro {
            continue;
        }
        // The macro body closes at the first unindented brace.
        if line == "}" {
            break;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('(')
            && let Some(ident) = rest.strip_suffix(") => {")
            && is_ident(ident)
        {
            arms.insert(ident);
        }
    }
    assert!(
        !arms.is_empty(),
        "found no `(ident) => {{` arms in __bind_host_cap_via_wit_bindgen!; \
         the parser or the macro layout changed",
    );
    arms
}

/// The capability list the zero-argument `bind_host_via_wit_bindgen!()`
/// form expands to: the sole non-comment `caps: [...]` occurrence whose
/// brackets carry plain idents rather than a `$cap` matcher.
fn blanket_caps_list(source: &str) -> Vec<String> {
    let mut lists = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        // Doc comments carry a `caps: [chain, logging]` usage example;
        // only real code lines count.
        if trimmed.starts_with("//") {
            continue;
        }
        let Some(after) = trimmed.split_once("caps: [").map(|(_, rest)| rest) else {
            continue;
        };
        let Some((body, _)) = after.split_once(']') else {
            continue;
        };
        if body.contains('$') {
            // The matcher arm `caps: [$($cap:ident),* $(,)?]`.
            continue;
        }
        let idents: Vec<String> = body
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        assert!(
            !idents.is_empty() && idents.iter().all(|s| is_ident(s)),
            "malformed blanket caps list: {body:?}",
        );
        lists.push(idents);
    }
    assert_eq!(
        lists.len(),
        1,
        "expected exactly one literal `caps: [...]` list in the bind macro, found {lists:?}",
    );
    lists.pop().unwrap()
}

/// Whether `s` is a plain snake_case ident, the only shape a bind-macro
/// capability takes.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
