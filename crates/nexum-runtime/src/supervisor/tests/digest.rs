//! Component digest pinning and verification, plus the compile-path guard.

use super::*;

/// The committed byte-stable `.wat` fixture and the manifest pinning its sha256.
fn pinned_fixture() -> (PathBuf, PathBuf) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/pinned");
    (dir.join("component.wat"), dir.join("module.toml"))
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

    let engine = make_wasmtime_engine();
    let declared = wrong_digest();
    let err = read_verified_component(&engine, &path, Some(&declared), false)
        .err()
        .expect("a mismatched digest must refuse the component");
    let mismatch = err
        .downcast_ref::<DigestMismatch>()
        .expect("the error is the typed mismatch");
    assert_eq!(mismatch.declared, declared);
    assert_eq!(
        mismatch.actual,
        ContentDigest::of_bytes(b"not the pinned bytes"),
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("component digest mismatch"), "{msg}");
    assert!(
        !msg.contains("compile"),
        "the mismatch must land before any compile: {msg}",
    );
}

#[test]
fn read_verified_component_requires_a_digest_when_the_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unpinned.wasm");
    std::fs::write(&path, b"any bytes at all").expect("write artifact");

    let engine = make_wasmtime_engine();
    let err = read_verified_component(&engine, &path, None, true)
        .err()
        .expect("an unpinned artifact must refuse under the flag");
    let msg = format!("{err:#}");
    assert!(msg.contains("require_component_digest"), "{msg}");
    assert!(!msg.contains("compile"), "refusal precedes compile: {msg}");
}

#[test]
fn read_verified_component_verifies_the_committed_pinned_fixture() {
    let (wat, manifest) = pinned_fixture();
    let loaded = manifest::load(&manifest, &CapabilityRegistry::core())
        .expect("the committed fixture manifest loads");
    let declared = loaded
        .component_digest
        .expect("the fixture manifest carries a pin");

    let engine = make_wasmtime_engine();
    let (_component, actual) = read_verified_component(&engine, &wat, Some(&declared), true)
        .expect("the pinned fixture verifies and compiles");
    assert_eq!(actual, declared);
}

#[test]
fn read_verified_component_computes_a_digest_for_unpinned_loads() {
    let (wat, _manifest) = pinned_fixture();
    let engine = make_wasmtime_engine();
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

/// Walk the supervisor's production sources, refuse any `Component::from_file`,
/// and collect the file names that compile a component. Recurses so a nested
/// module cannot host an unpinned compile path; test sources are skipped.
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
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        format!(
            "[module]\nname = \"pinned\"\ncomponent = \"{}\"\n\n\
             [capabilities]\nrequired = []\n",
            wrong_digest(),
        ),
    )
    .expect("write manifest");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let err = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .err()
    .expect("a stale pin must refuse the boot");
    let msg = format!("{err:#}");
    assert!(msg.contains("component digest mismatch"), "{msg}");
    assert!(
        !msg.contains("compile"),
        "the mismatch must precede any compile: {msg}",
    );
}

#[tokio::test]
async fn boot_single_requires_a_digest_when_the_engine_flag_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"unpinned\"\n\n[capabilities]\nrequired = []\n",
    )
    .expect("write manifest");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let err = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        true,
        &core_extensions(),
        None,
    )
    .await
    .err()
    .expect("an unpinned manifest must refuse under the flag");
    let msg = format!("{err:#}");
    assert!(msg.contains("require_component_digest"), "{msg}");
    assert!(!msg.contains("compile"), "refusal precedes compile: {msg}");
}

#[tokio::test]
async fn e2e_boot_single_accepts_a_matching_pinned_digest() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let digest = ContentDigest::of_bytes(&std::fs::read(&wasm).expect("read example wasm"));
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        format!(
            "[module]\nname = \"example\"\ncomponent = \"{digest}\"\n\n\
             [capabilities]\nrequired = [\"logging\"]\n",
        ),
    )
    .expect("write manifest");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        true,
        &core_extensions(),
        None,
    )
    .await
    .expect("a matching pin must boot under the strict flag");
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn boot_refuses_a_provider_with_a_mismatched_digest() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("acme.wasm");
    std::fs::write(&wasm, b"drifted provider bytes").expect("write artifact");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        format!(
            "[module]\nname = \"acme\"\nkind = \"acme-adapter\"\n\
             component = \"{}\"\n\n[capabilities]\nrequired = [\"chain\"]\n",
            wrong_digest(),
        ),
    )
    .expect("write manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: wasm,
            manifest: Some(manifest),
            http_allow: Vec::new(),
            messaging_topics: Vec::new(),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("a stale provider pin must refuse the boot"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(msg.contains("component digest mismatch"), "{msg}");
    assert!(!msg.contains("compile"), "refusal precedes compile: {msg}");
}

#[tokio::test]
async fn boot_requires_a_provider_digest_when_the_engine_flag_is_set() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("acme.wasm");
    std::fs::write(&wasm, b"unpinned provider bytes").expect("write artifact");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"acme\"\nkind = \"acme-adapter\"\n\n\
         [capabilities]\nrequired = [\"chain\"]\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        engine: crate::engine_config::EngineSection {
            require_component_digest: true,
            ..Default::default()
        },
        adapters: vec![crate::engine_config::AdapterEntry {
            path: wasm,
            manifest: Some(manifest),
            http_allow: Vec::new(),
            messaging_topics: Vec::new(),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("an unpinned adapter must refuse under the flag"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(msg.contains("require_component_digest"), "{msg}");
}

#[tokio::test]
async fn boot_requires_a_module_digest_when_the_engine_flag_is_set() {
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("module.wasm");
    std::fs::write(&wasm, b"unpinned artifact bytes").expect("write artifact");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"unpinned\"\n\n[capabilities]\nrequired = []\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        engine: crate::engine_config::EngineSection {
            require_component_digest: true,
            ..Default::default()
        },
        modules: vec![crate::engine_config::ModuleEntry {
            path: wasm,
            manifest: Some(manifest),
        }],
        ..Default::default()
    };

    let err = match Supervisor::boot(
        &engine,
        &linker,
        &config,
        &components,
        &core_extensions(),
        None,
    )
    .await
    {
        Ok(_) => panic!("an unpinned module must refuse under the flag"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("require_component_digest"), "{msg}");
    assert!(!msg.contains("compile"), "refusal precedes compile: {msg}");
}
