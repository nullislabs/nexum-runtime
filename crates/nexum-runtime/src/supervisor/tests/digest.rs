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
    let err = read_verified_component(&engine, &path, Some(&declared), false)
        .err()
        .expect("a mismatched digest must refuse the component");
    let crate::refusal::Refusal::Digest(mismatch) = &err else {
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
fn read_verified_component_requires_a_digest_when_the_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unpinned.wasm");
    std::fs::write(&path, b"any bytes at all").expect("write artifact");

    let engine = test_wasmtime_engine();
    let err = read_verified_component(&engine, &path, None, true)
        .err()
        .expect("an unpinned artifact must refuse under the flag");
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
    let (_component, actual) = read_verified_component(&engine, &wat, Some(&declared), true)
        .expect("the pinned fixture verifies and compiles");
    assert_eq!(actual, declared);
}

#[test]
fn read_verified_component_computes_a_digest_for_unpinned_loads() {
    let (wat, _manifest) = pinned_fixture();
    let engine = test_wasmtime_engine();
    let (_component, actual) =
        read_verified_component(&engine, &wat, None, false).expect("unpinned load compiles");
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
            e.declared == wrong_digest()
                && e.actual == ContentDigest::of_bytes(b"drifted artifact bytes")
        })
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
async fn boot_refuses_a_service_with_a_mismatched_digest() {
    let scenario = BootScenario::over(mock_components()).extensions(acme_extensions());
    let wasm = scenario.dir().join("acme.wasm");
    std::fs::write(&wasm, b"drifted service bytes").expect("write artifact");
    scenario
        .adapter(
            Entry::new(
                TestManifest::new("acme-adapter")
                    .kind("service")
                    .cap("chain")
                    .component_digest(wrong_digest().to_string()),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<DigestMismatch>(|e| {
            e.declared == wrong_digest()
                && e.actual == ContentDigest::of_bytes(b"drifted service bytes")
        })
        .lacks("compile");
}

#[tokio::test]
async fn boot_requires_a_service_digest_when_the_engine_flag_is_set() {
    let scenario = BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .require_digest();
    let wasm = scenario.dir().join("acme.wasm");
    std::fs::write(&wasm, b"unpinned service bytes").expect("write artifact");
    scenario
        .adapter(
            Entry::new(
                TestManifest::new("acme-adapter")
                    .kind("service")
                    .cap("chain"),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }));
}

#[tokio::test]
async fn boot_requires_a_module_digest_when_the_engine_flag_is_set() {
    let scenario = BootScenario::new().require_digest();
    let wasm = scenario.dir().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    scenario
        .module(Entry::new(TestManifest::new("unpinned")).wasm(wasm))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::DigestUnpinned { .. }))
        .lacks("compile");
}
