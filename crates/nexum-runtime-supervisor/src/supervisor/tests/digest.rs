//! Component digest pinning and verification, plus the compile-path guard.

use super::*;

/// The committed byte-stable `.wat` fixture and the manifest pinning its sha256.
fn pinned_fixture() -> (PathBuf, PathBuf) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/pinned");
    (dir.join("component.wat"), dir.join("component.toml"))
}

fn wrong_digest() -> ContentDigest {
    format!("sha256:{}", "1".repeat(64))
        .parse()
        .expect("a syntactically valid non-matching pin parses")
}

#[test]
fn read_verified_component_rejects_a_mismatched_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tampered.wasm");
    std::fs::write(&path, b"not the pinned bytes").expect("write artifact");

    let engine = test_wasmtime_engine();
    let declared = wrong_digest();
    let err = read_verified_component(&engine, &path, DigestPolicy::author(Some(&declared)))
        .err()
        .expect("a mismatched digest must refuse the component");
    let crate::error::RuntimeError::Digest(mismatch) = &err else {
        panic!("the refusal is the typed mismatch arm: {err}");
    };
    assert_eq!(mismatch.declared, declared);
    assert_eq!(
        mismatch.actual,
        ContentDigest::of_bytes(b"not the pinned bytes"),
    );
    Refusal::from(err)
        // Operator wording pin.
        .names("component digest mismatch")
        .lacks("compile");
}

#[test]
fn read_verified_component_rejects_a_mismatched_operator_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tampered.wasm");
    std::fs::write(&path, b"not the pinned bytes").expect("write artifact");

    let engine = test_wasmtime_engine();
    let declared = wrong_digest();
    let pins = DigestPolicy {
        operator: Some(&declared),
        author: None,
        require_operator: false,
    };
    let err = read_verified_component(&engine, &path, pins)
        .err()
        .expect("a mismatched operator pin must refuse the component");
    Refusal::from(err)
        .variant::<DigestMismatch>(|e| {
            e.pin == nexum_primitives::digest::DigestPin::Operator && e.declared == declared
        })
        // Operator wording pin: the fix is in engine.toml, not the manifest.
        .names("[[modules]].digest in engine.toml")
        .lacks("compile");
}

#[test]
fn read_verified_component_requires_an_operator_pin_and_reports_the_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unpinned.wasm");
    std::fs::write(&path, b"any bytes at all").expect("write artifact");

    let engine = test_wasmtime_engine();
    let expected = ContentDigest::of_bytes(b"any bytes at all");
    let pins = DigestPolicy {
        operator: None,
        author: None,
        require_operator: true,
    };
    let err = read_verified_component(&engine, &path, pins)
        .err()
        .expect("an unpinned entry must refuse under the requirement");
    Refusal::from(err)
        .variant::<LoadRefusal>(
            |e| matches!(e, LoadRefusal::DigestUnpinned { actual, .. } if *actual == expected),
        )
        // Operator wording pin: the value pastes out of the message, and
        // the message names the file and key it goes in.
        .names(&expected.to_string())
        .names("[[modules]]")
        .lacks("compile");
}

#[test]
fn read_verified_component_requires_an_operator_pin_despite_an_author_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-pinned.wasm");
    std::fs::write(&path, b"author pinned bytes").expect("write artifact");

    let engine = test_wasmtime_engine();
    let matching = ContentDigest::of_bytes(b"author pinned bytes");
    let pins = DigestPolicy {
        operator: None,
        author: Some(&matching),
        require_operator: true,
    };
    let err = read_verified_component(&engine, &path, pins)
        .err()
        .expect("a matching author pin must not satisfy the operator requirement");
    Refusal::from(err)
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}

/// So the unpinned refusal never tells an operator to paste a digest that a
/// pin already on disk contradicts.
#[test]
fn read_verified_component_reports_an_author_mismatch_before_the_missing_operator_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tampered.wasm");
    std::fs::write(&path, b"not the pinned bytes").expect("write artifact");

    let engine = test_wasmtime_engine();
    let declared = wrong_digest();
    let pins = DigestPolicy {
        operator: None,
        author: Some(&declared),
        require_operator: true,
    };
    let err = read_verified_component(&engine, &path, pins)
        .err()
        .expect("a mismatched author pin must refuse the component");
    Refusal::from(err)
        .variant::<DigestMismatch>(|e| {
            e.pin == nexum_primitives::digest::DigestPin::Author && e.declared == declared
        })
        .lacks("carries no digest")
        .lacks("compile");
}

#[test]
fn read_verified_component_accepts_an_operator_pin_alone() {
    let (wat, _manifest) = pinned_fixture();
    let bytes = std::fs::read(&wat).expect("read fixture");
    let operator = ContentDigest::of_bytes(&bytes);

    let engine = test_wasmtime_engine();
    let pins = DigestPolicy {
        operator: Some(&operator),
        author: None,
        require_operator: true,
    };
    let (_component, actual) = read_verified_component(&engine, &wat, pins)
        .expect("a matching operator pin satisfies the requirement");
    assert_eq!(actual, operator);
}

#[test]
fn read_verified_component_verifies_the_committed_pinned_fixture() {
    let (wat, manifest) = pinned_fixture();
    let loaded = manifest::load(&manifest, &CapabilityRegistry::core())
        .expect("the committed fixture manifest loads");
    let declared = loaded
        .component_digest
        .expect("the fixture manifest carries a pin");

    let engine = test_wasmtime_engine();
    let (_component, actual) =
        read_verified_component(&engine, &wat, DigestPolicy::author(Some(&declared)))
            .expect("the pinned fixture verifies and compiles");
    assert_eq!(actual, declared);
}

#[test]
fn read_verified_component_computes_a_digest_for_unpinned_loads() {
    let (wat, _manifest) = pinned_fixture();
    let engine = test_wasmtime_engine();
    let (_component, actual) = read_verified_component(&engine, &wat, DigestPolicy::author(None))
        .expect("unpinned load compiles");
    let bytes = std::fs::read(&wat).expect("read fixture");
    assert_eq!(actual, ContentDigest::of_bytes(&bytes));
}

/// A stray `Component::from_file` would reopen the artifact-swap window,
/// and a compile call outside artifact.rs would bypass digest verification.
/// The walk covers every workspace member, because the compile path is
/// reachable from `nexum-runtime-wasm` and `nexum-runtime` as well.
#[test]
fn no_production_component_from_file_call_remains() {
    let mut compile_sites = Vec::new();
    for root in workspace_source_roots() {
        collect_compile_sites(&root, &mut compile_sites);
    }
    // Sorted so a second site fails with a stable message; `read_dir` order
    // is filesystem-defined.
    compile_sites.sort();
    assert_eq!(
        compile_sites,
        ["crates/nexum-runtime-supervisor/src/supervisor/artifact.rs"],
        "the only production compile call must live in artifact.rs",
    );
}

/// The `src` of every workspace member, read from the root manifest so a crate
/// added tomorrow is walked without editing a list here. Enumerating nothing
/// means the members table moved rather than that the workspace is clean, so
/// each step refuses instead of passing vacuously; a walk that shrinks past
/// the supervisor also drops `artifact.rs` and fails the equality above.
fn workspace_source_roots() -> Vec<PathBuf> {
    let root = workspace_root();
    let raw =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read the workspace manifest");
    let manifest =
        toml::from_str::<toml::Table>(&raw).expect("the workspace manifest is valid TOML");
    let members = manifest["workspace"]["members"]
        .as_array()
        .expect("`workspace.members` is an array of member paths");
    assert!(
        !members.is_empty(),
        "enumerated no workspace members; the members table moved",
    );
    members
        .iter()
        .map(|member| {
            let member = member.as_str().expect("a member path is a string");
            let src = root.join(member).join("src");
            assert!(src.is_dir(), "workspace member {member} has no src to walk");
            src
        })
        .collect()
}

/// Members are named relative to the root, which sits two levels above a
/// `crates/<name>` manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root is two levels above this crate manifest")
        .to_path_buf()
}

/// Recurses so a nested module cannot host an unpinned compile path; test
/// sources are skipped. Sites are workspace-relative, since a bare file name
/// no longer says which crate it came from.
fn collect_compile_sites(dir: &Path, sites: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read a crate source directory") {
        let path = entry.expect("directory entry").path();
        let name = path
            .file_name()
            .expect("source entry name")
            .to_string_lossy()
            .into_owned();
        if matches!(
            name.as_str(),
            "tests" | "tests.rs" | "test_utils" | "test_utils.rs"
        ) {
            continue;
        }
        if path.is_dir() {
            collect_compile_sites(&path, sites);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read a crate source file");
        let site = path
            .strip_prefix(workspace_root())
            .expect("a walked file lives under the workspace root")
            .to_string_lossy()
            .into_owned();
        assert!(
            !src.contains("Component::from_file("),
            "{site} must compile components only via read_verified_component",
        );
        if src.contains("compile_component(") {
            sites.push(site);
        }
    }
}

#[tokio::test]
async fn boot_single_refuses_a_mismatched_component_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("module.wasm");
    std::fs::write(&wasm, b"drifted artifact bytes").expect("write artifact");
    let manifest = TestManifest::new("pinned")
        .component_digest(wrong_digest().to_string())
        .write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    Refusal::from(result.err().expect("a stale pin must refuse the boot"))
        .variant::<DigestMismatch>(|e| {
            e.pin == nexum_primitives::digest::DigestPin::Author
                && e.declared == wrong_digest()
                && e.actual == ContentDigest::of_bytes(b"drifted artifact bytes")
        })
        // The refusal names the file to edit; the operator pin lives elsewhere.
        .names("[component].digest in the manifest")
        .lacks("compile");
}

/// The production single-wasm caller passes `false`; that exemption is
/// pinned in `nexum-runtime`.
#[tokio::test]
async fn boot_single_requires_an_operator_pin_when_the_engine_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    let manifest = TestManifest::new("unpinned").write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), true, None).await;
    Refusal::from(
        result
            .err()
            .expect("an entry with no operator pin must refuse under the flag"),
    )
    .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
    .lacks("compile");
}

#[tokio::test]
async fn e2e_boot_single_accepts_a_matching_author_pin() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let digest = ContentDigest::of_bytes(&std::fs::read(&wasm).expect("read example wasm"));
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = TestManifest::new("example")
        .cap("logging")
        .component_digest(digest.to_string())
        .write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    let supervisor = result.expect("a matching author pin must boot");
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn boot_refuses_a_mismatched_operator_pin_before_compile() {
    let scenario = scenario();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"drifted artifact bytes").expect("write artifact");
    scenario
        .module(
            Entry::new(TestManifest::new("pinned-wrong"))
                .wasm(wasm)
                .digest(wrong_digest()),
        )
        .expect_refusal()
        .await
        .variant::<DigestMismatch>(|e| {
            e.pin == nexum_primitives::digest::DigestPin::Operator
                && e.declared == wrong_digest()
                && e.actual == ContentDigest::of_bytes(b"drifted artifact bytes")
        })
        .names("[[modules]].digest in engine.toml")
        .lacks("compile");
}

/// At most one can match the bytes. The operator's expectation is reported.
#[tokio::test]
async fn disagreeing_operator_and_author_pins_refuse() {
    let scenario = scenario();
    let wasm = scenario.dir().join("torn.wasm");
    std::fs::write(&wasm, b"torn pin bytes").expect("write artifact");
    let actual = ContentDigest::of_bytes(b"torn pin bytes");
    scenario
        .module(
            Entry::new(TestManifest::new("torn-pins").component_digest(actual.to_string()))
                .wasm(wasm)
                .digest(wrong_digest()),
        )
        .expect_refusal()
        .await
        .variant::<DigestMismatch>(|e| {
            e.pin == nexum_primitives::digest::DigestPin::Operator
                && e.declared == wrong_digest()
                && e.actual == actual
        })
        .lacks("compile");
}

#[tokio::test]
async fn e2e_boot_accepts_a_matching_operator_pin() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let digest = ContentDigest::of_bytes(&std::fs::read(&wasm).expect("read example wasm"));
    let booted = scenario()
        .require_digest()
        .module(
            Entry::new(TestManifest::new("example").cap("logging"))
                .wasm(wasm)
                .digest(digest),
        )
        .boot()
        .await
        .expect("a matching operator pin satisfies the requirement and boots");
    assert_eq!(booted.supervisor.alive_count(), 1);
}

#[tokio::test]
async fn boot_requires_a_module_digest_when_the_engine_flag_is_set() {
    let scenario = scenario().require_digest();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    let expected = ContentDigest::of_bytes(b"unpinned artifact bytes");
    scenario
        .module(Entry::new(TestManifest::new("unpinned")).wasm(wasm))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(
            |e| matches!(e, LoadRefusal::DigestUnpinned { actual, .. } if *actual == expected),
        )
        .names(&expected.to_string())
        .lacks("compile");
}

#[tokio::test]
async fn boot_requires_an_operator_pin_despite_a_matching_manifest_pin() {
    let scenario = scenario().require_digest();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"author pinned bytes").expect("write artifact");
    let matching = ContentDigest::of_bytes(b"author pinned bytes");
    scenario
        .module(
            Entry::new(TestManifest::new("author-pinned").component_digest(matching.to_string()))
                .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}

/// The bytes are not a component, so the boot still fails past the gate.
#[tokio::test]
async fn a_matching_operator_pin_clears_the_gate_with_no_manifest_pin() {
    let scenario = scenario().require_digest();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"operator pinned bytes").expect("write artifact");
    let matching = ContentDigest::of_bytes(b"operator pinned bytes");
    scenario
        .module(
            Entry::new(TestManifest::new("operator-pinned"))
                .wasm(wasm)
                .digest(matching),
        )
        .expect_refusal()
        .await
        .names("compile")
        .lacks("carries no digest");
}
