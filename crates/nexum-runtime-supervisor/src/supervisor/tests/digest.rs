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
    let err = read_verified_component(&engine, &path, DigestPolicy::author(Some(&declared), false))
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
        require_author: false,
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
fn read_verified_component_requires_a_digest_when_the_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unpinned.wasm");
    std::fs::write(&path, b"any bytes at all").expect("write artifact");

    let engine = test_wasmtime_engine();
    let err = read_verified_component(&engine, &path, DigestPolicy::author(None, true))
        .err()
        .expect("an unpinned artifact must refuse under the flag");
    Refusal::from(err)
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}

#[test]
fn read_verified_component_requires_an_author_pin_despite_an_operator_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("operator-pinned.wasm");
    std::fs::write(&path, b"operator pinned bytes").expect("write artifact");

    let engine = test_wasmtime_engine();
    let matching = ContentDigest::of_bytes(b"operator pinned bytes");
    let pins = DigestPolicy {
        operator: Some(&matching),
        author: None,
        require_author: true,
    };
    let err = read_verified_component(&engine, &path, pins)
        .err()
        .expect("a matching operator pin must not satisfy the author-pin flag");
    Refusal::from(err)
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
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
        read_verified_component(&engine, &wat, DigestPolicy::author(Some(&declared), true))
            .expect("the pinned fixture verifies and compiles");
    assert_eq!(actual, declared);
}

#[test]
fn read_verified_component_computes_a_digest_for_unpinned_loads() {
    let (wat, _manifest) = pinned_fixture();
    let engine = test_wasmtime_engine();
    let (_component, actual) =
        read_verified_component(&engine, &wat, DigestPolicy::author(None, false))
            .expect("unpinned load compiles");
    let bytes = std::fs::read(&wat).expect("read fixture");
    assert_eq!(actual, ContentDigest::of_bytes(&bytes));
}

/// A stray `Component::from_file` would reopen the artifact-swap window,
/// and a compile call outside artifact.rs would bypass digest verification.
#[test]
fn no_production_component_from_file_call_remains() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/supervisor");
    let mut compile_sites = Vec::new();
    collect_compile_sites(&dir, &mut compile_sites);
    // Sorted so a second site fails with a stable message; `read_dir` order
    // is filesystem-defined.
    compile_sites.sort();
    assert_eq!(
        compile_sites,
        ["artifact.rs"],
        "the only production compile call must live in artifact.rs",
    );
}

/// Recurses so a nested module cannot host an unpinned compile path; test
/// sources are skipped.
fn collect_compile_sites(dir: &Path, sites: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read supervisor source directory") {
        let path = entry.expect("directory entry").path();
        let name = path
            .file_name()
            .expect("source entry name")
            .to_string_lossy()
            .into_owned();
        if name == "tests" || name == "tests.rs" {
            continue;
        }
        if path.is_dir() {
            collect_compile_sites(&path, sites);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read supervisor source file");
        assert!(
            !src.contains("Component::from_file("),
            "{name} must compile components only via read_verified_component",
        );
        if src.contains("compile_component(") {
            sites.push(name);
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

#[tokio::test]
async fn boot_single_requires_a_digest_when_the_engine_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    let manifest = TestManifest::new("unpinned").write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), true, None).await;
    Refusal::from(
        result
            .err()
            .expect("an unpinned manifest must refuse under the flag"),
    )
    .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
    .lacks("compile");
}

#[tokio::test]
async fn e2e_boot_single_accepts_a_matching_pinned_digest() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let digest = ContentDigest::of_bytes(&std::fs::read(&wasm).expect("read example wasm"));
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = TestManifest::new("example")
        .cap("logging")
        .component_digest(digest.to_string())
        .write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), true, None).await;
    let supervisor = result.expect("a matching pin must boot under the strict flag");
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

/// Both pins present and disagreeing: at most one matches the bytes, so
/// the artifact refuses; the operator's expectation is reported first.
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
        .module(
            Entry::new(TestManifest::new("example").cap("logging"))
                .wasm(wasm)
                .digest(digest),
        )
        .boot()
        .await
        .expect("a matching operator pin boots");
    assert_eq!(booted.supervisor.alive_count(), 1);
}

#[tokio::test]
async fn boot_requires_a_module_digest_when_the_engine_flag_is_set() {
    let scenario = scenario().require_digest();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    scenario
        .module(Entry::new(TestManifest::new("unpinned")).wasm(wasm))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}

#[tokio::test]
async fn boot_requires_a_manifest_pin_despite_a_matching_operator_pin() {
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
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}
