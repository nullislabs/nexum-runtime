//! Boot refusals: admission gates that reject a module or provider
//! before any compile.

use super::*;

/// An `[[adapters]]` entry whose manifest is (or defaults to) an
/// event-module is rejected before instantiation, naming the registered
/// kinds.
#[tokio::test]
async fn boot_rejects_provider_whose_manifest_is_an_event_module() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"acme\"\nkind = \"event-module\"\n\n\
         [capabilities]\nrequired = []\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: dir.path().join("acme.wasm"),
            manifest: Some(manifest),
            http_allow: Vec::new(),
            messaging_topics: Vec::new(),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("event-module manifest in an [[adapters]] slot must be rejected"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("acme-adapter"),
        "the kind gate names the registered kinds: {msg}",
    );
}

/// A kind spelling no extension registered is refused at boot with a
/// message naming the registered kinds.
#[tokio::test]
async fn boot_rejects_an_unregistered_provider_kind() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"bad\"\nkind = \"gadget\"\n\n[capabilities]\nrequired = []\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: dir.path().join("gadget.wasm"),
            manifest: Some(manifest),
            http_allow: Vec::new(),
            messaging_topics: Vec::new(),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("an unregistered provider kind must be refused"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unregistered provider kind gadget") && msg.contains("acme-adapter"),
        "the refusal names the unknown spelling and the registered kinds: {msg}",
    );
}

/// A registered kind clears the discriminator; boot then reaches the
/// component read step.
#[tokio::test]
async fn boot_admits_a_registered_provider_kind_past_the_kind_gate() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"acme\"\nkind = \"acme-adapter\"\n\n\
         [capabilities]\nrequired = [\"chain\"]\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: dir.path().join("missing-acme.wasm"),
            manifest: Some(manifest),
            http_allow: vec!["api.acme.example".into()],
            messaging_topics: vec!["/nexum/1/acme-orders/proto".into()],
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("absent provider wasm must fail the compile step"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("read component") && msg.contains("missing-acme"),
        "boot reached the component read step past the kind gate: {msg}",
    );
    assert!(
        !msg.contains("requires a module.toml"),
        "the kind gate passed rather than rejecting: {msg}",
    );
}

/// A module subscribing to an extension kind no wired extension declares
/// is refused at boot; `[capabilities]` is declared so the kind gate is
/// what fails.
#[tokio::test]
async fn boot_refuses_an_undeclared_extension_subscription_kind() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        r#"
[module]
name = "example"

[capabilities]
required = ["logging"]

[[subscription]]
kind = "acme-status"
"#,
    )
    .expect("write manifest");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let result = Supervisor::boot_single(
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
    .await;
    let err = result
        .err()
        .expect("an undeclared extension subscription kind must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown event kind acme-status"),
        "the refusal names the kind: {msg}",
    );
}

/// No module.toml anywhere refuses boot before compile; no wasm needs to
/// exist.
#[tokio::test]
async fn boot_refuses_a_component_without_module_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("orphan.wasm");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let err = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        None,
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .err()
    .expect("a component without any module.toml must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no module.toml") && msg.contains("orphan.wasm"),
        "the refusal names the component: {msg}",
    );
    assert!(
        msg.contains("required = []"),
        "the refusal carries the migration hint: {msg}",
    );
}

#[tokio::test]
async fn boot_refuses_a_nonexistent_explicit_manifest_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("mod.wasm");
    let manifest = dir.path().join("modle.toml");

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
    .expect("a nonexistent explicit manifest path must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("modle.toml") && msg.contains("not found"),
        "the refusal names the missing manifest path: {msg}",
    );
}

/// Operator `http_allow` must not stand in for the provider's own
/// `[capabilities]` declaration.
#[tokio::test]
async fn boot_refuses_a_capsless_provider_manifest_despite_operator_http_allow() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"acme\"\nkind = \"acme-adapter\"\n",
    )
    .expect("write manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: dir.path().join("acme.wasm"),
            manifest: Some(manifest),
            http_allow: vec!["api.acme.example".into()],
            messaging_topics: Vec::new(),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("a caps-less provider manifest must be refused"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no [capabilities] section"),
        "the refusal is the manifest load error, not a later gate: {msg}",
    );
}

/// The missing-capabilities refusal precedes the kind and section gates
/// and the compile.
#[tokio::test]
async fn capsless_manifest_reports_missing_capabilities_before_other_gates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("mod.wasm");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        r#"
[module]
name = "example"

[venue]
body_version = 2

[[subscription]]
kind = "acme-status"
"#,
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
    .expect("a caps-less manifest must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no [capabilities] section"),
        "the manifest refusal fires first: {msg}",
    );
    assert!(
        msg.contains("required = []"),
        "the refusal states the two-line fix: {msg}",
    );
    assert!(
        !msg.contains("unknown event kind") && !msg.contains("no wired extension claims"),
        "the later gates must not be reached: {msg}",
    );
}

/// Only `chain` is undeclared, so the refusal is deterministic regardless
/// of import order.
#[tokio::test]
async fn boot_denies_an_undeclared_chain_import_for_balance_tracker() {
    let Some(wasm) = module_wasm_or_skip("balance-tracker") else {
        return;
    };
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        r#"
[module]
name = "balance-tracker"

[capabilities]
required = ["logging", "local-store"]

[[subscription]]
kind = "block"
chain_id = 1
"#,
    )
    .expect("write manifest");
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
    .expect("undeclared imports must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("capability violation"),
        "the boot error is a capability violation: {msg}",
    );
    assert!(
        msg.contains("nexum:host/chain"),
        "the violation names the withheld chain import: {msg}",
    );
}
