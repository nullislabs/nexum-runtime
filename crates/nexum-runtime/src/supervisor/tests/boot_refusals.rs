//! Boot refusals: admission gates that reject before any compile.

use super::*;

// The counter's label mapping and the closed `error_kind` set are pinned
// by the tests in `crate::error`, on `RuntimeError` values rather than on
// a downcast chain.

/// The real counter at the real call site: a boot through the supervisor
/// increments `nexum_runtime_boot_refusals_total` under the refusal's
/// split ParseError class, with the `error_kind` label key intact.
#[test]
fn a_boot_refusal_increments_the_counter_under_its_parse_class() {
    use crate::test_utils::metrics_util::debugging::DebugValue;

    use crate::test_utils::{capture_metrics, samples_named};

    // Raw TOML: the textual absence of [dependencies] is the fixture.
    let manifest = "[component]\nname = \"example\"\n";
    let (refusal, samples) = capture_metrics(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(
                BootScenario::new()
                    .module(manifest.to_owned())
                    .expect_refusal(),
            )
    });
    refusal.variant::<BootRefusal>(|e| {
        matches!(e, BootRefusal::Manifest(ParseError::MissingCapabilities))
    });
    let hits = samples_named(&samples, "nexum_runtime_boot_refusals_total");
    assert_eq!(hits.len(), 1, "one series: {samples:?}");
    assert!(
        hits[0].has_label("error_kind", "missing_capabilities"),
        "{:?}",
        hits[0].labels,
    );
    assert!(
        matches!(hits[0].value, DebugValue::Counter(1)),
        "{:?}",
        hits[0].value,
    );
}

/// `[dependencies]` is declared so the failing gate is the trigger kind.
#[tokio::test]
async fn boot_refuses_an_undeclared_extension_trigger_kind() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .extension_trigger("acme-status", &[]),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(
            |e| matches!(e, LoadRefusal::UnknownTriggerKind { kind, .. } if kind == "acme-status"),
        );
}

/// No wasm needs to exist; the refusal precedes compile and carries the
/// migration hint.
#[tokio::test]
async fn boot_refuses_a_component_without_a_manifest() {
    let scenario = BootScenario::new();
    let orphan = scenario.dir().join("orphan.wasm");
    scenario
        .module(Entry::new(ManifestInput::Beside).wasm(orphan))
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::ManifestMissing { component }
                if component.ends_with("orphan.wasm"))
        })
        // Operator wording pin.
        .names("empty [dependencies] table grants")
        .lacks("compile");
}

#[tokio::test]
async fn boot_refuses_a_nonexistent_explicit_manifest_path() {
    let scenario = BootScenario::new();
    let missing = scenario.dir().join("modle.toml");
    scenario
        .module(missing)
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::ManifestNotFound { manifest, .. }
                if manifest.ends_with("modle.toml"))
        });
}

/// A manifest without `[dependencies]` refuses before the trigger-kind
/// gate and before any compile.
#[tokio::test]
async fn boot_refuses_a_capsless_manifest_before_any_other_gate() {
    // Raw TOML: the textual absence of [dependencies] is the fixture.
    let module = "[component]\nname = \"example\"\n\n\
                  [venue]\nbody_version = 2\n\n\
                  [[trigger]]\non = \"acme-status\"\n";
    BootScenario::new()
        .module(module.to_owned())
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::MissingCapabilities))
        })
        // Operator wording pin.
        .names("empty one grants nothing")
        .lacks("unknown trigger kind")
        .lacks("no wired extension claims")
        .lacks("compile");
}

/// A blank name is refused at parse, and no refusal reaches the component
/// read.
#[tokio::test]
async fn boot_refuses_a_blank_manifest_name() {
    for blank in ["  ", "\t", "\n"] {
        BootScenario::new()
            .module(TestManifest::new(blank).cap("logging"))
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(e, BootRefusal::Manifest(ParseError::BlankModuleName))
            })
            .lacks("claimed twice")
            .lacks("read component")
            .lacks("compile");
    }
}

/// Only `chain` is undeclared, so the refusal is deterministic regardless
/// of import order.
#[tokio::test]
async fn boot_denies_an_undeclared_chain_import_for_balance_tracker() {
    let Some(wasm) = module_wasm_or_skip("balance-tracker") else {
        return;
    };
    BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("balance-tracker")
                .cap("logging")
                .cap("local-store")
                .block_trigger(1),
        )
        .expect_refusal()
        .await
        .variant::<CapabilityError>(|e| {
            matches!(e, CapabilityError::Undeclared(v)
                if v.capability == "chain" && v.wit_import.starts_with("nexum:host/chain"))
        });
}

/// `[policy].capabilities` is the ceiling on what a manifest may declare;
/// the refusal precedes any component read.
#[tokio::test]
async fn boot_refuses_a_capability_the_policy_excludes() {
    BootScenario::new()
        .policy(PolicySection {
            capabilities: Some(vec!["chain".to_owned()]),
            ..PolicySection::default()
        })
        .module(TestManifest::new("example").cap("logging"))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::CapabilityNotPermitted { id, capability, .. }
                if id == "m0" && capability == "logging")
        })
        .lacks("read component")
        .lacks("compile");
}

/// Chain data arrives through `on_trigger` rather than an import, so a
/// permitted set that excludes `chain` refuses a chain trigger too.
#[tokio::test]
async fn boot_refuses_a_chain_trigger_the_policy_excludes() {
    BootScenario::new()
        .policy(PolicySection {
            capabilities: Some(vec!["logging".to_owned()]),
            ..PolicySection::default()
        })
        .module(TestManifest::new("example").cap("logging").block_trigger(1))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ChainTriggerNotPermitted { id, permitted }
                if id == "m0" && permitted == "logging")
        })
        .lacks("compile");
}

/// A `[policy.component]` row that permits the declared set admits the
/// component under the same global allowlist that refuses its sibling.
#[tokio::test]
async fn a_component_policy_row_overrides_the_global_capability_set() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    BootScenario::new()
        .wasm(wasm)
        .policy(PolicySection {
            capabilities: Some(vec!["chain".to_owned()]),
            component: [(
                "m0".to_owned(),
                ComponentPolicy {
                    capabilities: Some(vec!["logging".to_owned()]),
                    ..ComponentPolicy::default()
                },
            )]
            .into(),
            ..PolicySection::default()
        })
        .module(TestManifest::new("example").cap("logging"))
        .boot()
        .await
        .expect("the row permits what the global set refuses");
}

/// Two in-ceiling components still refuse together when their declared
/// reservations cross `[policy.total]`; the refusal names the second.
#[tokio::test]
async fn boot_refuses_an_overcommitted_component_set() {
    BootScenario::new()
        .policy(PolicySection {
            total: TotalPolicy {
                // One default 64 MiB reservation fits, two do not.
                max_memory_bytes: std::num::NonZeroUsize::new(100 * 1024 * 1024),
            },
            ..PolicySection::default()
        })
        .module(TestManifest::new("a").cap("logging"))
        .module(TestManifest::new("b").cap("logging"))
        .expect_refusal()
        .await
        .variant::<BootRefusal>(
            |e| matches!(e, BootRefusal::TotalMemoryExceeded { id, .. } if id == "m1"),
        )
        .lacks("read component")
        .lacks("compile");
}
