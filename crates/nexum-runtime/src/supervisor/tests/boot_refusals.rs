//! Boot refusals: admission gates that reject before any compile.

use super::*;

// The counter's label mapping and the closed `error_kind` set are pinned
// by the tests in `crate::refusal`, on `Refusal` values rather than on a
// downcast chain.

/// The real counter at the real call site: a boot through the supervisor
/// increments `nexum_runtime_boot_refusals_total` under the refusal's
/// split ParseError class, with the `error_kind` label key intact.
#[test]
fn a_boot_refusal_increments_the_counter_under_its_parse_class() {
    use metrics_util::debugging::DebugValue;

    use crate::test_utils::metrics_capture::{capture_metrics, samples_named};

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

/// `[dependencies]` is declared so the failing gate is the subscription kind.
#[tokio::test]
async fn boot_refuses_an_undeclared_extension_subscription_kind() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("example")
                .cap("logging")
                .extension_sub("acme-status", &[]),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(
            |e| matches!(e, LoadRefusal::UnknownEventKind { kind, .. } if kind == "acme-status"),
        );
}

/// No wasm needs to exist; the refusal precedes compile and carries the
/// migration hint.
#[tokio::test]
async fn boot_refuses_a_component_without_a_manifest() {
    let scenario = BootScenario::new();
    let orphan = scenario.dir().join("orphan.wasm");
    scenario
        .module(Entry::new(ManifestSource::Beside).wasm(orphan))
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

/// A manifest without `[dependencies]` refuses before the subscription-kind
/// gate and before any compile.
#[tokio::test]
async fn boot_refuses_a_capsless_manifest_before_any_other_gate() {
    // Raw TOML: the textual absence of [dependencies] is the fixture.
    let module = "[component]\nname = \"example\"\n\n\
                  [venue]\nbody_version = 2\n\n\
                  [[subscription]]\nkind = \"acme-status\"\n";
    BootScenario::new()
        .module(module.to_owned())
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::MissingCapabilities))
        })
        // Operator wording pin.
        .names("empty one grants nothing")
        .lacks("unknown event kind")
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
                .block_sub(1),
        )
        .expect_refusal()
        .await
        .variant::<CapabilityError>(|e| {
            matches!(e, CapabilityError::Undeclared(v)
                if v.capability == "chain" && v.wit_import.starts_with("nexum:host/chain"))
        });
}
