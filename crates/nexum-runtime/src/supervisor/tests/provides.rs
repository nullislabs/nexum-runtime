//! The `provides` claim: export verification, the `[implements]`
//! binding, the operator pin, and the prepass duplicate-claim gate.

use wasmtime::component::types::ComponentItem;

use super::*;
use crate::digest::DigestPin;
use crate::interface_id::InterfaceId;
use crate::supervisor::load::enforce_provides;

/// The committed fixture that exports an interface instance alongside the
/// event-module surface.
fn provider_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/provides/component.wat")
}

fn fixture_digest(path: &Path) -> ContentDigest {
    ContentDigest::of_bytes(&std::fs::read(path).expect("read fixture"))
}

/// The empty component from the pinned fixture: compiles, exports nothing.
fn empty_component_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/pinned/component.wat")
}

fn claim(value: &str) -> InterfaceId {
    InterfaceId::parse(value).expect("valid claim")
}

/// Type-level exports of a compiled component, as `enforce_provides` sees
/// them.
fn exports_of(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
) -> Vec<(String, ComponentItem)> {
    component
        .component_type()
        .exports(engine)
        .map(|(name, export)| (name.to_owned(), export.ty))
        .collect()
}

fn compile(engine: &wasmtime::Engine, path: &Path) -> wasmtime::component::Component {
    let (component, _digest) =
        read_verified_component(engine, path, DigestPolicy::author(None, false))
            .expect("fixture compiles");
    component
}

#[test]
fn a_satisfying_interface_instance_export_passes_the_walk() {
    let engine = test_wasmtime_engine();
    let component = compile(&engine, &provider_fixture());
    let exports = exports_of(&engine, &component);
    // In track and no older than the claim.
    for ok in [
        "nexum:fixture/provider@1.0.0",
        "nexum:fixture/provider@1.2.3",
    ] {
        enforce_provides(
            "m0",
            &provider_fixture(),
            &claim(ok),
            exports.iter().map(|(n, item)| (n.as_str(), item.clone())),
        )
        .expect("the export satisfies the claim");
    }
}

#[test]
fn a_wrong_track_or_newer_claim_is_refused_naming_the_near_miss() {
    let engine = test_wasmtime_engine();
    let component = compile(&engine, &provider_fixture());
    let exports = exports_of(&engine, &component);
    // The export is @1.2.3: another track, and a claim above it, both lie.
    for bad in [
        "nexum:fixture/provider@2.0.0",
        "nexum:fixture/provider@1.3.0",
    ] {
        let err = enforce_provides(
            "m0",
            &provider_fixture(),
            &claim(bad),
            exports.iter().map(|(n, item)| (n.as_str(), item.clone())),
        )
        .expect_err("an unsatisfied claim must refuse");
        assert!(
            matches!(&err, LoadRefusal::ProvidesNotExported { id, claimed, exported, .. }
                if id == "m0" && claimed == bad
                    && exported.contains("nexum:fixture/provider@1.2.3")),
            "{err}",
        );
    }
}

/// A component may export a bare func under an interface-shaped name;
/// only an instance export satisfies a claim.
#[test]
fn a_func_export_under_the_claimed_name_does_not_satisfy_the_claim() {
    const FUNC_ONLY: &str = r#"
(component
  (core module $m (func (export "f") (result i32) (i32.const 0)))
  (core instance $i (instantiate $m))
  (alias core export $i "f" (core func $fc))
  (func $f (result u32) (canon lift (core func $fc)))
  (export "acme:iface/thing@1.0.0" (func $f))
)
"#;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("func-only.wat");
    std::fs::write(&path, FUNC_ONLY).expect("write fixture");
    let engine = test_wasmtime_engine();
    let component = compile(&engine, &path);
    let exports = exports_of(&engine, &component);
    let err = enforce_provides(
        "m0",
        &path,
        &claim("acme:iface/thing@1.0.0"),
        exports.iter().map(|(n, item)| (n.as_str(), item.clone())),
    )
    .expect_err("a bare func must not satisfy an interface claim");
    assert!(
        matches!(&err, LoadRefusal::ProvidesNotExported { exported, .. } if exported == "none"),
        "{err}",
    );
}

#[tokio::test]
async fn two_claimants_of_one_interface_refuse_in_prepass_naming_both_paths() {
    let scenario = BootScenario::new();
    let (first, second) = (
        scenario.dir().join("first-claimant.wasm"),
        scenario.dir().join("second-claimant.wasm"),
    );
    // Two full versions on one track; neither artifact exists, so the
    // refusal provably precedes any read or compile.
    scenario
        .module(
            Entry::new(
                TestManifest::new("claimant-a")
                    .cap("logging")
                    .provides("acme:pool/quoter@2.0.0"),
            )
            .wasm(first),
        )
        .module(
            Entry::new(
                TestManifest::new("claimant-b")
                    .cap("logging")
                    .provides("acme:pool/quoter@2.1.0"),
            )
            .wasm(second),
        )
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::InterfaceClaimed { interface, held, path }
                if interface.as_str() == "acme:pool/quoter@2"
                    && held.ends_with("first-claimant.wasm")
                    && path.ends_with("second-claimant.wasm"))
        })
        .names("first-claimant.wasm")
        .names("second-claimant.wasm")
        .lacks("compile");
}

#[tokio::test]
async fn an_implementer_absent_from_implements_does_not_load() {
    let scenario = BootScenario::new();
    let wasm = scenario.dir().join("claimant.wasm");
    scenario
        .module(
            Entry::new(
                TestManifest::new("claimant")
                    .cap("logging")
                    .provides("acme:pool/quoter@2.0.0"),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ImplementerUnbound { id, interface, bound }
                if id == "m0" && interface == "acme:pool/quoter@2" && bound == "nothing")
        })
        // The artifact does not exist: the refusal precedes any read.
        .lacks("read component");
}

#[tokio::test]
async fn an_implementer_bound_to_another_id_does_not_load() {
    let scenario = BootScenario::new();
    let wasm = scenario.dir().join("claimant.wasm");
    scenario
        .implement("acme:pool/quoter@2", "the-authorized-one", None)
        .module(
            Entry::new(
                TestManifest::new("claimant")
                    .cap("logging")
                    .provides("acme:pool/quoter@2.0.0"),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ImplementerUnbound { id, bound, .. }
                if id == "m0" && bound == "the-authorized-one")
        })
        .names("the-authorized-one");
}

#[tokio::test]
async fn an_implementer_without_an_operator_digest_does_not_load() {
    let scenario = BootScenario::new();
    let wasm = scenario.dir().join("claimant.wasm");
    scenario
        .implement("acme:pool/quoter@2", "m0", None)
        .module(
            Entry::new(
                TestManifest::new("claimant")
                    .cap("logging")
                    .provides("acme:pool/quoter@2.0.0"),
            )
            .wasm(wasm),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ImplementerUnpinned { id, interface }
                if id == "m0" && interface == "acme:pool/quoter@2")
        })
        .lacks("read component");
}

/// `boot_single` never runs the prepass, so the binding gate must hold on
/// its path too.
#[tokio::test]
async fn boot_single_refuses_an_unbound_provides_claimant() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = dir.path().join("claimant.wasm");
    let manifest = TestManifest::new("claimant")
        .provides("acme:pool/quoter@2.0.0")
        .write_to(dir.path());

    let (_store, result) = try_boot_single(&wasm, Some(&manifest), false, None).await;
    Refusal::from(result.err().expect("an unbound claimant must refuse"))
        .variant::<LoadRefusal>(|e| matches!(e, LoadRefusal::ImplementerUnbound { .. }))
        .lacks("read component");
}

#[tokio::test]
async fn a_claim_the_component_does_not_export_is_refused_naming_the_claim() {
    let fixture = empty_component_fixture();
    let scenario = BootScenario::new().implement(
        "nexum:fixture/provider@1",
        "m0",
        Some(fixture_digest(&fixture)),
    );
    scenario
        .module(
            Entry::new(
                TestManifest::new("false-claimant").provides("nexum:fixture/provider@1.0.0"),
            )
            .wasm(fixture),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ProvidesNotExported { id, claimed, exported, .. }
                if id == "m0"
                    && claimed == "nexum:fixture/provider@1.0.0"
                    && exported == "none")
        })
        .names("nexum:fixture/provider@1.0.0");
}

/// The operator's row carries the only operator-written pin on the
/// artifact. Dropping one line of the untrusted manifest must not drop
/// that pin with it, so a row naming an entry that claims nothing
/// refuses rather than going inert.
#[tokio::test]
async fn dropping_the_claim_does_not_disarm_the_operator_pin() {
    let fixture = provider_fixture();
    let stale: ContentDigest = format!("sha256:{}", "4".repeat(64))
        .parse()
        .expect("valid non-matching pin");
    BootScenario::new()
        // The operator reviewed and pinned m0's artifact.
        .implement("nexum:fixture/provider@1", "m0", Some(stale))
        // The author ships other bytes and deletes `provides`.
        .module(Entry::new(TestManifest::new("silent-claimant")).wasm(fixture))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ImplementerNotClaiming { id, interface, claimed }
                if id == "m0"
                    && interface == "nexum:fixture/provider@1"
                    && claimed == "nothing")
        })
        .lacks("read component");
}

/// The same hole through a claim on another interface: the row must
/// match the entry's own claim, not merely coexist with one.
#[tokio::test]
async fn a_row_the_entrys_claim_does_not_match_refuses() {
    let fixture = provider_fixture();
    BootScenario::new()
        .implement("acme:pool/quoter@2", "m0", Some(fixture_digest(&fixture)))
        .implement(
            "nexum:fixture/provider@1",
            "m0",
            Some(fixture_digest(&fixture)),
        )
        .module(
            Entry::new(
                TestManifest::new("one-claim-two-rows").provides("nexum:fixture/provider@1.0.0"),
            )
            .wasm(fixture),
        )
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::ImplementerNotClaiming { interface, claimed, .. }
                if interface == "acme:pool/quoter@2"
                    && claimed == "nexum:fixture/provider@1")
        });
}

/// The whole positive path: the name types nothing, the claim is
/// verified against the real exports, and the operator's binding plus
/// pin authorize the load.
#[tokio::test]
async fn a_component_whose_name_differs_from_its_interface_id_still_loads() {
    let fixture = provider_fixture();
    let booted = BootScenario::new()
        .implement(
            "nexum:fixture/provider@1",
            "m0",
            Some(fixture_digest(&fixture)),
        )
        .module(
            Entry::new(
                TestManifest::new("a-name-unlike-the-interface")
                    .provides("nexum:fixture/provider@1.0.0"),
            )
            .wasm(fixture),
        )
        .boot()
        .await
        .expect("a bound, pinned, true claim boots");
    assert_eq!(booted.supervisor.module_count(), 1);
    assert_eq!(booted.supervisor.alive_count(), 1);
}

#[tokio::test]
async fn a_mismatched_operator_pin_refuses_before_compile() {
    let fixture = provider_fixture();
    let wrong: ContentDigest = format!("sha256:{}", "2".repeat(64))
        .parse()
        .expect("valid non-matching pin");
    BootScenario::new()
        .implement("nexum:fixture/provider@1", "m0", Some(wrong))
        .module(
            Entry::new(TestManifest::new("pinned-wrong").provides("nexum:fixture/provider@1.0.0"))
                .wasm(fixture),
        )
        .expect_refusal()
        .await
        .variant::<DigestMismatch>(|e| e.pin == DigestPin::Operator && e.declared == wrong)
        // The operator edits engine.toml, not the author's manifest.
        .names("[implements] digest in engine.toml")
        .lacks("compile");
}

/// Both pins present and disagreeing: at most one matches the bytes, so
/// the artifact refuses; the operator's expectation is reported first.
#[tokio::test]
async fn disagreeing_operator_and_author_pins_refuse() {
    let fixture = provider_fixture();
    let actual = fixture_digest(&fixture);
    let wrong: ContentDigest = format!("sha256:{}", "3".repeat(64))
        .parse()
        .expect("valid non-matching pin");
    BootScenario::new()
        .implement("nexum:fixture/provider@1", "m0", Some(wrong))
        .module(
            Entry::new(
                TestManifest::new("torn-pins")
                    .component_digest(actual.to_string())
                    .provides("nexum:fixture/provider@1.0.0"),
            )
            .wasm(fixture),
        )
        .expect_refusal()
        .await
        .variant::<DigestMismatch>(|e| {
            e.pin == DigestPin::Operator && e.declared == wrong && e.actual == actual
        })
        .lacks("compile");
}

/// A single claimant passes the prepass ledger; the ledger only refuses
/// the second claimant of one track.
#[test]
fn claim_interface_holds_distinct_tracks_apart() {
    let mut ledger = super::prepass::InterfaceLedger::new();
    super::prepass::claim_interface(&mut ledger, &claim("a:b/c@1.0.0"), Path::new("a.wasm"))
        .expect("first claim");
    super::prepass::claim_interface(&mut ledger, &claim("a:b/c@2.0.0"), Path::new("b.wasm"))
        .expect("another track is another claim");
    let err =
        super::prepass::claim_interface(&mut ledger, &claim("a:b/c@2.9.9"), Path::new("c.wasm"))
            .expect_err("one track, one implementer");
    assert!(
        matches!(&err, BootRefusal::InterfaceClaimed { interface, held, path }
            if interface.as_str() == "a:b/c@2"
                && held.as_path() == Path::new("b.wasm")
                && path.as_path() == Path::new("c.wasm")),
        "the refusal names both claimants: {err}",
    );
}

/// `BootScenario` bypasses the raw-config conversion, so this pins the
/// serde path an operator actually takes: `[implements]` parses and its
/// claimant boots.
#[test]
fn an_implements_row_parses_from_engine_toml_to_the_typed_section() {
    let toml = r#"
[implements."nexum:fixture/provider@1"]
component = "m0"
digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[modules]]
id = "m0"
path = "m0.wasm"
"#;
    let cfg: crate::engine_config::EngineConfig =
        toml::from_str(toml).expect("[implements] parses");
    let track = crate::interface_id::InterfaceTrack::parse("nexum:fixture/provider@1")
        .expect("valid track");
    let row = cfg.implements.get(&track).expect("row resolved");
    assert_eq!(row.component, "m0");
    assert!(row.digest.is_some());
}
