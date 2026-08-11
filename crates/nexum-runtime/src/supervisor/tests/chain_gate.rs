//! The configured-chains gate and the chain-facing subscription surface.

use super::*;

#[tokio::test]
async fn empty_supervisor_returns_no_subscriptions() {
    let booted = BootScenario::over(mock_components())
        .boot()
        .await
        .expect("an empty scenario boots");
    let plan = booted.supervisor.subscription_plan();
    assert!(plan.block_chains.is_empty());
    assert!(plan.chain_log_subs.is_empty());
    assert_eq!(plan.viability(0), Viability::Nothing);
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
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::UnconfiguredChain { noun: "module", name, chain_id: 424_242, .. }
                    if name == "example")
            })
            // Operator wording pin.
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
    .variant::<BootRefusal>(|e| {
        matches!(e, BootRefusal::UnconfiguredChain { noun: "module", name, chain_id: 424_242, .. }
            if name == "gated")
    })
    // Operator wording pin.
    .names("module gated subscribes to chain 424242")
    .names("configured chains: 1, 100, 11155111")
    .lacks("compile");
}

/// The gate covers `[[services]]` entries too: a provider manifest cannot
/// subscribe past the operator's `[chains]` set.
#[tokio::test]
async fn boot_refuses_an_adapter_subscription_on_an_unconfigured_chain() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(
            TestManifest::new("acme-adapter")
                .kind("service")
                .cap("chain")
                .block_sub(424_242),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChain { noun: "service", name, chain_id: 424_242, .. }
                if name == "acme-adapter")
        })
        // Operator wording pin.
        .names("load service")
        .names("service acme-adapter subscribes to chain 424242")
        .names("[chains.424242]")
        .lacks("read component")
        .lacks("compile");
}

/// Filter values fail closed at manifest parse: an unparseable address or
/// topic refuses the boot as a manifest error, before any compile.
#[tokio::test]
async fn boot_refuses_an_invalid_chain_log_filter() {
    for (manifest, detail) in [
        (
            TestManifest::new("example")
                .cap("logging")
                .chain_log_sub_filtered(1, Some("0xabc"), None),
            // Pinned operator wording.
            "invalid chain-log address \"0xabc\"",
        ),
        (
            TestManifest::new("example")
                .cap("logging")
                .chain_log_sub_filtered(1, None, Some("not-a-topic")),
            // Pinned operator wording.
            "invalid topic \"not-a-topic\"",
        ),
    ] {
        BootScenario::new()
            .module(manifest)
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| matches!(e, BootRefusal::Manifest(ParseError::Toml(_))))
            // Operator wording pin.
            .names("load module")
            .names("manifest: parse")
            .names(detail)
            .lacks("read component")
            .lacks("compile");
    }
}

/// The manifest carries typed filter values, so the collection-time filter
/// build cannot fail.
#[tokio::test]
async fn a_validated_chain_log_filter_survives_to_the_collected_subscription() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let address = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let topic = "0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
    let booted = BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .chain_log_sub_filtered(1, Some(address), Some(topic)),
        )
        .boot()
        .await
        .expect("the example boots alive");

    let subs = booted.supervisor.subscription_plan().chain_log_subs;
    assert_eq!(
        subs.len(),
        1,
        "the alive module contributes its subscription"
    );
    assert_eq!(subs[0].module.as_str(), "example");
    assert_eq!(subs[0].chain.id(), 1);
    assert!(subs[0].cursor_key.is_none(), "resume defaults to off");
    // alloy `Filter` exposes no getter; assert through its serialization.
    let serialized = serde_json::to_value(&subs[0].filter).unwrap().to_string();
    assert!(
        serialized
            .to_lowercase()
            .contains(&address.to_lowercase()[2..]),
        "{serialized}",
    );
    assert!(serialized.contains(&topic[2..]), "{serialized}");
}

#[tokio::test]
async fn boot_admits_a_block_subscription_on_a_configured_chain_past_the_chain_gate() {
    BootScenario::new()
        .module(TestManifest::new("example").cap("logging").block_sub(1))
        .expect_refusal()
        .await
        .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound)
        // Operator wording pin.
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChain { noun: "module", name, chain_id: 424_242, .. }
                if name == "example")
        })
        // Operator wording pin.
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChainDefaulted { noun: "module", name, chain_id: 424_242 }
                if name == "example")
        })
        // Operator wording pin.
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
    let msg = unconfigured_chain(Role::Module, "example", 424_242, &chains).to_string();
    // Operator wording pin.
    assert!(msg.contains("configured chains: none"), "{msg}");
    assert!(!msg.contains("no engine.toml was found"), "{msg}");
}
