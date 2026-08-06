//! The configured-chains gate and the chain-facing subscription surface.

use super::*;

#[tokio::test]
async fn empty_supervisor_returns_no_subscriptions() {
    let booted = BootScenario::over(mock_components())
        .boot()
        .await
        .expect("an empty scenario boots");
    assert!(booted.supervisor.block_chains().is_empty());
    assert!(booted.supervisor.chain_log_subscriptions().is_empty());
    assert_eq!(booted.supervisor.module_count(), 0);
}

/// The refusal precedes compile; block and chain-log subscriptions hit the
/// same gate with the same wording.
#[tokio::test]
async fn boot_refuses_a_subscription_on_an_unconfigured_chain() {
    for manifest in [
        TestManifest::new("example")
            .cap("logging")
            .block_sub(424_242),
        TestManifest::new("example")
            .cap("logging")
            .chain_log_sub(424_242),
    ] {
        BootScenario::new()
            .module(manifest)
            .expect_refusal()
            .await
            .names("module example subscribes to chain 424242")
            .names("[chains.424242]")
            .names("configured chains: 1, 100, 11155111")
            .lacks("compile");
    }
}

/// The single-boot path reads the same configured-chains gate.
#[tokio::test]
async fn boot_single_refuses_a_subscription_on_an_unconfigured_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = TestManifest::new("gated")
        .cap("logging")
        .block_sub(424_242)
        .write_to(dir.path());
    let wasm = dir.path().join("missing.wasm");

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    Refusal::from(
        result
            .err()
            .expect("an unconfigured chain must refuse the boot"),
    )
    .names("module gated subscribes to chain 424242")
    .names("configured chains: 1, 100, 11155111")
    .lacks("compile");
}

#[tokio::test]
async fn boot_admits_a_block_subscription_on_a_configured_chain_past_the_chain_gate() {
    BootScenario::new()
        .module(TestManifest::new("example").cap("logging").block_sub(1))
        .expect_refusal()
        .await
        .names("read component")
        .lacks("subscribes to chain");
}

#[tokio::test]
async fn an_unconfigured_chain_refuses_boot_before_an_earlier_module_loads() {
    let scenario = BootScenario::new();
    let second = scenario.dir().join("second.wasm");
    scenario
        .module(TestManifest::new("first").cap("logging"))
        .module(
            Entry::new(
                TestManifest::new("example")
                    .cap("logging")
                    .block_sub(424_242),
            )
            .wasm(second),
        )
        .expect_refusal()
        .await
        .names("load module")
        .names("second.wasm")
        .names("module example subscribes to chain 424242")
        .names("[chains.424242]")
        .lacks("compile");
}

#[tokio::test]
async fn boot_refusal_names_the_missing_engine_toml_on_the_defaulted_path() {
    BootScenario::new()
        .defaulted_chains()
        .module(
            TestManifest::new("example")
                .cap("logging")
                .block_sub(424_242),
        )
        .expect_refusal()
        .await
        .names("no engine.toml was found")
        .names("[chains.424242]")
        .lacks("configured chains:");
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
