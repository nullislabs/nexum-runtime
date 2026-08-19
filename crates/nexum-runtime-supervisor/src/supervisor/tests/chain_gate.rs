//! The configured-chains gate and the chain-facing trigger surface.

use super::*;

#[tokio::test]
async fn empty_supervisor_returns_an_empty_source_plan() {
    let booted = BootScenario::over(mock_components())
        .boot()
        .await
        .expect("an empty scenario boots");
    let plan = booted.supervisor.source_plan();
    assert!(plan.block_chains.is_empty());
    assert!(plan.event_sources.is_empty());
    assert_eq!(plan.viability(0), Viability::Nothing);
    assert_eq!(booted.supervisor.module_count(), 0);
}

/// The refusal precedes compile; block and event triggers hit the
/// same gate with the same wording.
#[tokio::test]
async fn boot_refuses_a_trigger_on_an_unconfigured_chain() {
    for manifest in [
        TestManifest::new("example")
            .cap("logging")
            .block_trigger(424_242),
        TestManifest::new("example")
            .cap("logging")
            .event_trigger(424_242),
    ] {
        scenario()
            .module(manifest)
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::UnconfiguredChain { name, chain_id: 424_242, .. }
                    if name == "example")
            })
            // Operator wording pin.
            .names("module example declares a trigger on chain 424242")
            .names("[chains.424242]")
            .names("configured chains: 1, 100, 11155111")
            .lacks("compile");
    }
}

/// The single-boot path reads the same configured-chains gate.
#[tokio::test]
async fn boot_single_refuses_a_trigger_on_an_unconfigured_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = TestManifest::new("gated")
        .cap("logging")
        .block_trigger(424_242)
        .write_to(dir.path());
    let wasm = dir.path().join("missing.wasm");

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    Refusal::from(
        result
            .err()
            .expect("an unconfigured chain must refuse the boot"),
    )
    .variant::<BootRefusal>(|e| {
        matches!(e, BootRefusal::UnconfiguredChain { name, chain_id: 424_242, .. }
            if name == "gated")
    })
    // Operator wording pin.
    .names("module gated declares a trigger on chain 424242")
    .names("configured chains: 1, 100, 11155111")
    .lacks("compile");
}

/// Filter values fail closed at manifest parse: an unparseable address or
/// topic refuses the boot as a manifest error, before any compile.
#[tokio::test]
async fn boot_refuses_an_invalid_event_filter() {
    fn is_address(e: &BootRefusal) -> bool {
        matches!(
            e,
            BootRefusal::Manifest(ParseError::InvalidEventAddress { .. })
        )
    }
    fn is_topic(e: &BootRefusal) -> bool {
        matches!(
            e,
            BootRefusal::Manifest(ParseError::InvalidEventTopic { .. })
        )
    }
    for (manifest, detail, variant) in [
        (
            TestManifest::new("example")
                .cap("logging")
                .event_trigger_filtered(1, Some("0xabc"), None),
            // Pinned operator wording.
            "invalid event address \"0xabc\"",
            is_address as fn(&BootRefusal) -> bool,
        ),
        (
            TestManifest::new("example")
                .cap("logging")
                .event_trigger_filtered(1, None, Some("not-a-topic")),
            // Pinned operator wording.
            "invalid topic \"not-a-topic\"",
            is_topic as fn(&BootRefusal) -> bool,
        ),
    ] {
        scenario()
            .module(manifest)
            .expect_refusal()
            .await
            .variant::<BootRefusal>(variant)
            // Operator wording pin.
            .names("load module")
            .names(detail)
            .lacks("read component")
            .lacks("compile");
    }
}

/// The manifest carries typed filter values, so the collection-time filter
/// build cannot fail.
#[tokio::test]
async fn a_validated_event_filter_survives_to_the_collected_stream() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let address = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let topic = "0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
    let booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .event_trigger_filtered(1, Some(address), Some(topic)),
        )
        .boot()
        .await
        .expect("the example boots alive");

    let sources = booted.supervisor.source_plan().event_sources;
    assert_eq!(sources.len(), 1, "the alive module contributes its stream");
    assert_eq!(sources[0].module.as_str(), "example");
    assert_eq!(sources[0].chain.id(), 1);
    assert!(sources[0].cursor_key.is_none(), "resume defaults to off");
    // alloy `Filter` exposes no getter; assert through its serialization.
    let serialized = serde_json::to_value(&sources[0].filter)
        .unwrap()
        .to_string();
    assert!(
        serialized
            .to_lowercase()
            .contains(&address.to_lowercase()[2..]),
        "{serialized}",
    );
    assert!(serialized.contains(&topic[2..]), "{serialized}");
}

#[tokio::test]
async fn boot_admits_a_block_trigger_on_a_configured_chain_past_the_chain_gate() {
    scenario()
        .module(TestManifest::new("example").cap("logging").block_trigger(1))
        .expect_refusal()
        .await
        .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound)
        // Operator wording pin.
        .names("read component")
        .lacks("declares a trigger on chain");
}

#[tokio::test]
async fn an_unconfigured_chain_refuses_boot_before_an_earlier_module_loads() {
    let scenario = scenario();
    let second = scenario.dir().join("second.wasm");
    scenario
        .module(TestManifest::new("first").cap("logging"))
        .module(
            Entry::new(
                TestManifest::new("example")
                    .cap("logging")
                    .block_trigger(424_242),
            )
            .wasm(second),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChain { name, chain_id: 424_242, .. }
                if name == "example")
        })
        // Operator wording pin.
        .names("load module")
        .names("second.wasm")
        .names("module example declares a trigger on chain 424242")
        .names("[chains.424242]")
        .lacks("compile");
}

#[tokio::test]
async fn boot_refusal_names_the_missing_engine_toml_on_the_defaulted_path() {
    scenario()
        .defaulted_chains()
        .module(
            TestManifest::new("example")
                .cap("logging")
                .block_trigger(424_242),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::UnconfiguredChainDefaulted { name, chain_id: 424_242 }
                if name == "example")
        })
        // Operator wording pin.
        .names("no engine.toml was found")
        .names("[chains.424242]")
        .lacks("configured chains:");
}

#[test]
fn configured_chains_normalise_named_and_numeric_spellings() {
    let cfg =
        toml::from_str::<EngineConfig>("[chains.sepolia]\nrpc_url = \"http://localhost:8545\"\n")
            .expect("named chain key parses");
    let chains = ConfiguredChains::from_config(&cfg);
    assert!(chains.contains(11_155_111));
    assert!(!chains.contains(1));
}

#[test]
fn unconfigured_chain_message_says_none_when_engine_toml_declares_no_chains() {
    let chains = ConfiguredChains::from_config(&EngineConfig::default());
    let msg = unconfigured_chain("example", 424_242, &chains).to_string();
    // Operator wording pin.
    assert!(msg.contains("configured chains: none"), "{msg}");
    assert!(!msg.contains("no engine.toml was found"), "{msg}");
}
