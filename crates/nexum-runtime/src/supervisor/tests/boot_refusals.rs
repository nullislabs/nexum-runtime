//! Boot refusals: admission gates that reject before any compile.

use super::*;

/// Rejected before instantiation, naming the registered kinds; a manifest
/// without a kind defaults to an event-module.
#[tokio::test]
async fn boot_rejects_provider_whose_manifest_is_an_event_module() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(TestManifest::new("acme").kind("event-module"))
        .expect_refusal()
        .await
        .names("acme-adapter");
}

/// The refusal names the registered kinds.
#[tokio::test]
async fn boot_rejects_an_unregistered_provider_kind() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(TestManifest::new("bad").kind("gadget"))
        .expect_refusal()
        .await
        .names("unregistered provider kind gadget")
        .names("acme-adapter");
}

/// A registered kind clears the discriminator; boot reaches the component
/// read step.
#[tokio::test]
async fn boot_admits_a_registered_provider_kind_past_the_kind_gate() {
    let scenario = BootScenario::over(mock_components()).extensions(acme_extensions());
    let missing = scenario.dir().join("missing-acme.wasm");
    scenario
        .adapter(
            Entry::new(TestManifest::new("acme").kind("acme-adapter").cap("chain"))
                .wasm(missing)
                .http_allow(["api.acme.example"])
                .messaging_topics(["/nexum/1/acme-orders/proto"]),
        )
        .expect_refusal()
        .await
        .names("read component")
        .names("missing-acme")
        .lacks("requires a module.toml");
}

/// `[capabilities]` is declared so the failing gate is the subscription kind.
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
        .names("unknown event kind acme-status");
}

/// No wasm needs to exist; the refusal precedes compile and carries the
/// migration hint.
#[tokio::test]
async fn boot_refuses_a_component_without_module_toml() {
    let scenario = BootScenario::new();
    let orphan = scenario.dir().join("orphan.wasm");
    scenario
        .module(Entry::new(ManifestSource::Beside).wasm(orphan))
        .expect_refusal()
        .await
        .names("no module.toml")
        .names("orphan.wasm")
        .names("required = []")
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
        .names("modle.toml")
        .names("not found");
}

/// Operator `http_allow` must not stand in for the component's own
/// `[capabilities]`; only the module path runs the kind gate, so it carries the decoy.
#[tokio::test]
async fn boot_refuses_a_capsless_manifest_before_any_other_gate() {
    // Raw TOML: the textual absence of [capabilities] is the fixture.
    let provider = "[module]\nname = \"acme\"\nkind = \"acme-adapter\"\n\n\
                    [venue]\nbody_version = 2\n\n\
                    [[subscription]]\nkind = \"acme-status\"\n";
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(Entry::new(provider.to_owned()).http_allow(["api.acme.example"]))
        .expect_refusal()
        .await
        .names("no [capabilities] section")
        .names("required = []")
        .lacks("no wired extension claims")
        .lacks("compile");

    let module = "[module]\nname = \"example\"\n\n\
                  [venue]\nbody_version = 2\n\n\
                  [[subscription]]\nkind = \"acme-status\"\n";
    BootScenario::new()
        .module(module.to_owned())
        .expect_refusal()
        .await
        .names("no [capabilities] section")
        .names("required = []")
        .lacks("unknown event kind")
        .lacks("no wired extension claims")
        .lacks("compile");
}

/// A blank name is refused at parse for both roles. Two blank-named
/// adapters refuse on the name, not as a second claim on a shared fallback
/// namespace, and no refusal reaches the component read.
#[tokio::test]
async fn boot_refuses_a_blank_manifest_name_for_both_roles() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(TestManifest::new("").kind("acme-adapter").cap("chain"))
        .adapter(TestManifest::new("").kind("acme-adapter").cap("chain"))
        .expect_refusal()
        .await
        .names("[module].name")
        .lacks("claimed twice")
        .lacks("read component")
        .lacks("compile");

    for blank in ["  ", "\t", "\n"] {
        BootScenario::new()
            .module(TestManifest::new(blank).cap("logging"))
            .expect_refusal()
            .await
            .names("[module].name")
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
        .names("capability violation")
        .names("nexum:host/chain");
}

/// The example component's only gated import is `logging`, so the refusal
/// is deterministic; the provider path holds the import to the declaration
/// just as the module path does.
#[tokio::test]
async fn boot_denies_an_undeclared_logging_import_for_a_provider() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(Entry::new(TestManifest::new("acme").kind("acme-adapter").cap("chain")).wasm(wasm))
        .expect_refusal()
        .await
        .names("capability violation")
        .names("nexum:host/logging");
}
