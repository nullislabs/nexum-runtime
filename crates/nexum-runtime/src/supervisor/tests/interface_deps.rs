//! Dependencies on provided interfaces: resolution against the whole
//! loaded set, refused in the prepass, before any compile.

use super::*;

/// The committed fixture that exports `nexum:fixture/provider@1.2.3`
/// alongside the event-module surface.
fn provider_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/provides/component.wat")
}

fn fixture_digest(path: &Path) -> ContentDigest {
    ContentDigest::of_bytes(&std::fs::read(path).expect("read fixture"))
}

/// The whole positive path: the consumer names a track, the provider
/// claims it, and the consumer may come first in `engine.toml` order.
#[tokio::test]
async fn a_dependency_on_a_provided_interface_boots() {
    let fixture = provider_fixture();
    let booted = BootScenario::new()
        .implement(
            "nexum:fixture/provider@1",
            "m1",
            Some(fixture_digest(&fixture)),
        )
        .module(
            Entry::new(
                TestManifest::new("consumer").interface_dep("wallet", "nexum:fixture/provider@1"),
            )
            .wasm(&fixture),
        )
        .module(
            Entry::new(TestManifest::new("quoter-svc").provides("nexum:fixture/provider@1.0.0"))
                .wasm(&fixture),
        )
        .boot()
        .await
        .expect("a dependency on a provided interface boots");
    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(booted.supervisor.alive_count(), 2);
}

/// A bareword naming the provider component refuses with the corrected
/// interface line, resolved against the whole ledger: the provider is a
/// later entry, and neither artifact exists.
#[tokio::test]
async fn a_dependency_naming_the_provider_component_refuses_with_the_corrected_line() {
    let scenario = BootScenario::new();
    let (consumer, provider) = (
        scenario.dir().join("consumer.wasm"),
        scenario.dir().join("quoter-svc.wasm"),
    );
    scenario
        .module(Entry::new(TestManifest::new("consumer").cap("quoter-svc")).wasm(consumer))
        .module(
            Entry::new(TestManifest::new("quoter-svc").provides("acme:pool/quoter@2.0.0"))
                .wasm(provider),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::DependencyNamesComponent {
                dependency, interface,
            }) if dependency == "quoter-svc" && interface.as_str() == "acme:pool/quoter@2")
        })
        .names("quoter-svc = { interface = \"acme:pool/quoter@2\" }")
        .lacks("compile");
}

/// A track no loaded component provides refuses at boot, blaming the
/// consumer entry, before any artifact is read or compiled.
#[tokio::test]
async fn a_track_no_component_provides_refuses_at_boot_blaming_the_consumer() {
    let scenario = BootScenario::new();
    let (consumer, provider) = (
        scenario.dir().join("consumer.wasm"),
        scenario.dir().join("signer-svc.wasm"),
    );
    scenario
        .module(
            Entry::new(TestManifest::new("consumer").interface_dep("wallet", "acme:pool/quoter@2"))
                .wasm(consumer),
        )
        .module(
            Entry::new(TestManifest::new("signer-svc").provides("nexum:wallet/signer@1.0.0"))
                .wasm(provider),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::InterfaceNotProvided {
                dependency, interface, provided,
            }) if dependency == "wallet"
                && interface.as_str() == "acme:pool/quoter@2"
                && provided == "nexum:wallet/signer@1")
        })
        .names("consumer.wasm")
        .lacks("compile");
}

/// A component whose own claim is the only provider of the track it
/// depends on refuses in the prepass: the self-loop is visible there and
/// must not defer to the call wiring.
#[tokio::test]
async fn a_dependency_on_the_component_own_interface_refuses_at_boot() {
    let scenario = BootScenario::new();
    let wasm = scenario.dir().join("quoter-svc.wasm");
    scenario
        .module(
            Entry::new(
                TestManifest::new("quoter-svc")
                    .provides("acme:pool/quoter@2.0.0")
                    .interface_dep("myself", "acme:pool/quoter@2"),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::SelfInterfaceDependency {
                dependency, interface,
            }) if dependency == "myself" && interface.as_str() == "acme:pool/quoter@2")
        })
        .lacks("compile");
}

/// One manifest refuses the same class on both boot paths: the dependency
/// cross-check precedes the configured-chains gate per entry on the
/// engine.toml path exactly as on `boot_single`.
#[tokio::test]
async fn both_boot_paths_refuse_the_dependency_class_first() {
    fn is_unknown(e: &BootRefusal) -> bool {
        matches!(
            e,
            BootRefusal::Manifest(ParseError::UnknownCapability { name, .. })
                if name == "telepathy"
        )
    }
    let manifest = || {
        TestManifest::new("both")
            .cap("telepathy")
            .block_sub(424_242)
    };

    let scenario = BootScenario::new();
    let wasm = scenario.dir().join("both.wasm");
    scenario
        .module(Entry::new(manifest()).wasm(wasm))
        .expect_refusal()
        .await
        .variant::<BootRefusal>(is_unknown);

    let dir = tempfile::tempdir().expect("tempdir");
    let single = manifest().write_to(dir.path());
    let wasm = dir.path().join("both.wasm");
    let (_store, result) = try_boot_single(&wasm, Some(&single), false, None).await;
    Refusal::from(result.err().expect("the unknown dependency must refuse"))
        .variant::<BootRefusal>(is_unknown);
}

/// `boot_single` never runs the prepass, so the resolution gate must hold
/// on its path too: a single entry has no provider beside itself.
#[tokio::test]
async fn boot_single_refuses_an_interface_dependency_without_a_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("consumer.wasm");
    let manifest = TestManifest::new("consumer")
        .interface_dep("wallet", "acme:pool/quoter@2")
        .write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    Refusal::from(result.err().expect("an unprovided track must refuse"))
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::InterfaceNotProvided { provided, .. })
                if provided == "none")
        })
        .lacks("compile");
}
