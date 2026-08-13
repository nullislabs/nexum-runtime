//! Ties [`nexum_world::CORE`] to the two surfaces it describes in string
//! literals: the `wit/nexum-host/` package and the SDK bind macro.
//! Nothing else resolves those literals at host-build time, so a rename
//! on either side would otherwise surface only in a guest build.
//!
//! Pinned to the `wit-parser` wasmtime v46 embeds, so the check cannot
//! disagree with the parser the runtime runs.

use std::collections::BTreeSet;
use std::path::Path;

use nexum_world::CORE;

/// Resolved from the manifest dir so the test is cwd-independent.
fn nexum_host_wit_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit/nexum-host")
}

/// Read as text: the idents are `macro_rules!` matcher literals, so text
/// is the only host-side surface they have.
fn sdk_macro_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nexum-sdk/src/wit_bindgen_macro.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// A WIT interface rename fails here rather than in the next guest build.
/// Also asserts a non-zero check count, so an empty parse cannot pass.
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

/// A capability with no bind arm, or an arm no capability reaches,
/// fails here. Checks both the arms and the blanket list.
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

/// The `(ident) => {` arms of `__bind_host_cap_via_wit_bindgen!`.
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

/// The blanket list: the one non-comment `caps: [...]` carrying plain
/// idents rather than a `$cap` matcher.
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
    lists.remove(0)
}

/// Plain snake_case, the only shape a bind-macro capability takes.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
