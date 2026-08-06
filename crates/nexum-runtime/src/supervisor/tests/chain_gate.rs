//! The configured-chains gate and the chain-facing subscription surface.

use super::*;

#[tokio::test]
async fn empty_supervisor_returns_no_subscriptions() {
    let engine = make_wasmtime_engine();
    let sup = boot_mock_supervisor(&engine).await;
    assert!(sup.block_chains().is_empty());
    assert!(sup.chain_log_subscriptions().is_empty());
    assert_eq!(sup.module_count(), 0);
}

/// Manifest subscribing to chain 424242 with the given kind line(s).
fn unconfigured_chain_manifest(dir: &Path, subscription: &str) -> PathBuf {
    let manifest = dir.join("module.toml");
    std::fs::write(
        &manifest,
        format!(
            "[module]\nname = \"example\"\n\n[capabilities]\nrequired = [\"logging\"]\n\n\
             [[subscription]]\n{subscription}\nchain_id = 424242\n"
        ),
    )
    .expect("write manifest");
    manifest
}

/// The refusal precedes compile; no wasm exists.
#[tokio::test]
async fn boot_refuses_a_block_subscription_on_an_unconfigured_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("missing.wasm");
    let manifest = unconfigured_chain_manifest(dir.path(), "kind = \"block\"");

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
    .expect("an unconfigured chain subscription must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("module example subscribes to chain 424242"),
        "the refusal names the module and the chain id: {msg}",
    );
    assert!(
        msg.contains("[chains.424242]"),
        "the refusal states the missing stanza: {msg}",
    );
    assert!(
        msg.contains("configured chains: 1, 100, 11155111"),
        "the refusal lists the configured set: {msg}",
    );
    assert!(
        !msg.contains("compile"),
        "the refusal precedes any compile of the missing wasm: {msg}",
    );
}

#[tokio::test]
async fn boot_refuses_a_chain_log_subscription_on_an_unconfigured_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("missing.wasm");
    let manifest = unconfigured_chain_manifest(dir.path(), "kind = \"chain-log\"");

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
    .expect("an unconfigured chain-log subscription must refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("module example subscribes to chain 424242")
            && msg.contains("[chains.424242]"),
        "the refusal names the module, the chain id, and the missing stanza: {msg}",
    );
}

#[tokio::test]
async fn boot_admits_a_block_subscription_on_a_configured_chain_past_the_chain_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("missing.wasm");
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        "[module]\nname = \"example\"\n\n[capabilities]\nrequired = [\"logging\"]\n\n\
         [[subscription]]\nkind = \"block\"\nchain_id = 1\n",
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
    .expect("the absent wasm must fail the component read step");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("read component"),
        "boot reached the component read step past the chain gate: {msg}",
    );
    assert!(
        !msg.contains("subscribes to chain"),
        "a configured chain must not trip the gate: {msg}",
    );
}

#[tokio::test]
async fn an_unconfigured_chain_refuses_boot_before_an_earlier_module_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first_wasm = dir.path().join("first.wasm");
    let first_manifest = dir.path().join("first.toml");
    std::fs::write(
        &first_manifest,
        "[module]\nname = \"first\"\n\n[capabilities]\nrequired = [\"logging\"]\n",
    )
    .expect("write first manifest");
    let second_wasm = dir.path().join("second.wasm");
    let second_manifest = unconfigured_chain_manifest(dir.path(), "kind = \"block\"");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let engine_cfg = EngineConfig {
        chains: crate::test_utils::test_chain_configs(),
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: first_wasm,
                manifest: Some(first_manifest),
            },
            crate::engine_config::ModuleEntry {
                path: second_wasm,
                manifest: Some(second_manifest),
            },
        ],
        ..Default::default()
    };

    let err = match Supervisor::boot(
        &engine,
        &linker,
        &engine_cfg,
        &components,
        &core_extensions(),
        None,
    )
    .await
    {
        Ok(_) => panic!("an unconfigured chain subscription must refuse the boot"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("load module") && msg.contains("second.wasm"),
        "the refusal carries the load-module context: {msg}",
    );
    assert!(
        msg.contains("module example subscribes to chain 424242")
            && msg.contains("[chains.424242]"),
        "the refusal is the chain gate: {msg}",
    );
    assert!(
        !msg.contains("compile"),
        "no earlier module reached the compile step: {msg}",
    );
}

#[tokio::test]
async fn boot_refusal_names_the_missing_engine_toml_on_the_defaulted_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("missing.wasm");
    let manifest = unconfigured_chain_manifest(dir.path(), "kind = \"block\"");

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();
    let defaulted_chains = ConfiguredChains::from_config(&EngineConfig {
        defaulted: true,
        ..EngineConfig::default()
    });

    let err = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &defaulted_chains,
        false,
        &core_extensions(),
        None,
    )
    .await
    .err()
    .expect("the defaulted path must still refuse boot");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no engine.toml was found"),
        "the refusal names the missing engine.toml: {msg}",
    );
    assert!(
        msg.contains("[chains.424242]"),
        "the refusal states the stanza to create: {msg}",
    );
    assert!(
        !msg.contains("configured chains:"),
        "the defaulted wording replaces the empty configured list: {msg}",
    );
}

#[test]
fn configured_chains_normalise_named_and_numeric_spellings() {
    let cfg: EngineConfig =
        toml::from_str("[chains.sepolia]\nrpc_url = \"http://localhost:8545\"\n")
            .expect("named chain key parses");
    let chains = ConfiguredChains::from_config(&cfg);
    assert!(chains.contains(11_155_111));
    assert!(!chains.contains(1));
}

#[test]
fn unconfigured_chain_message_says_none_when_engine_toml_declares_no_chains() {
    let chains = ConfiguredChains::from_config(&EngineConfig::default());
    let msg = crate::supervisor::unconfigured_chain("example", 424_242, &chains).to_string();
    assert!(msg.contains("configured chains: none"), "{msg}");
    assert!(!msg.contains("no engine.toml was found"), "{msg}");
}
