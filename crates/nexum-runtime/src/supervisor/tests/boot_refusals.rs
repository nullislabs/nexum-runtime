//! Boot refusals: admission gates that reject before any compile.

use super::*;

/// A root missing here is a refusal class no operator dashboard sees, so the
/// table is asserted through the same `with_context` wrap boot applies.
#[test]
fn every_typed_refusal_root_labels_the_counter_under_a_context_wrap() {
    let digest = ContentDigest::of_bytes(b"artifact");
    let cases: Vec<(anyhow::Error, &str)> = vec![
        (
            BootRefusal::ManifestMissing {
                component: PathBuf::from("orphan.wasm"),
            }
            .into(),
            "manifest_missing",
        ),
        (
            LoadRefusal::SectionClaimed { section: "venue" }.into(),
            "section_claimed",
        ),
        (
            CapabilityError::UnknownWasi {
                wit_import: "wasi:sockets/tcp@0.2.0".to_owned(),
            }
            .into(),
            "unknown_wasi",
        ),
        (
            DigestMismatch {
                path: PathBuf::from("pinned.wasm"),
                declared: digest,
                actual: digest,
            }
            .into(),
            "digest_mismatch",
        ),
    ];
    for (err, kind) in cases {
        let wrapped = err.context("module pinned.wasm");
        assert_eq!(boot_refusal_kind(&wrapped), Some(kind), "{wrapped:#}");
    }
}

/// An untyped refusal is counted under no kind rather than a wrong one.
#[test]
fn an_untyped_refusal_carries_no_counter_label() {
    assert_eq!(boot_refusal_kind(&anyhow::anyhow!("engine gone")), None);
}

/// Rejected before instantiation, naming the registered kinds; a manifest
/// without a kind defaults to an event-module.
#[tokio::test]
async fn boot_rejects_provider_whose_manifest_is_an_event_module() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(TestManifest::new("acme").kind("event-module"))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::WorkerKindAdapter { registered, .. }
                if registered == &["acme-adapter"])
        });
}

/// The refusal names the registered kinds.
#[tokio::test]
async fn boot_rejects_an_unregistered_provider_kind() {
    BootScenario::over(mock_components())
        .extensions(acme_extensions())
        .adapter(TestManifest::new("bad").kind("gadget"))
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(e, LoadRefusal::UnregisteredKind { kind, registered, .. }
                if kind == "gadget" && registered == &["acme-adapter"])
        })
        // Operator wording pin.
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
                .http_allow(["api.acme.example"]),
        )
        .expect_refusal()
        .await
        .variant::<std::io::Error>(|e| e.kind() == std::io::ErrorKind::NotFound)
        // Operator wording pin.
        .names("read component")
        .names("missing-acme")
        .lacks("requires a module.toml");
}

/// The multi-entry path wires provider kinds, so a serviceless kind refuses
/// the boot before any entry loads.
#[tokio::test]
async fn boot_refuses_a_provider_kind_without_a_host_service() {
    BootScenario::over(mock_components())
        .extensions(serviceless_acme_extensions())
        .expect_refusal()
        .await
        .variant::<LoadRefusal>(|e| {
            matches!(
                e,
                LoadRefusal::ServicelessKind {
                    namespace: "acme",
                    kind: "acme-adapter"
                }
            )
        })
        // Operator wording pin.
        .names("extension acme registers provider kind acme-adapter without a host service");
}

/// Provider kinds come only from `engine.toml`, so single boot skips the
/// service gate and the first refusal is the missing manifest.
#[tokio::test]
async fn boot_single_skips_the_provider_kind_service_gate() {
    let extensions = serviceless_acme_extensions();
    let engine = test_wasmtime_engine();
    let linker = crate::supervisor::build_linker(&engine, &extensions).expect("build_linker");
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = ModuleEntry {
        path: dir.path().join("missing.wasm"),
        manifest: None,
    };
    let limits = ModuleLimits::default();
    let env = BootEnv {
        limits: &limits,
        configured_chains: test_chains(),
        require_component_digest: false,
    };
    let err = Supervisor::boot_single(
        &engine,
        &linker,
        &entry,
        &mock_components(),
        &env,
        &extensions,
        None,
    )
    .await
    .err()
    .expect("a missing manifest must refuse the boot");
    Refusal::from(err)
        .variant::<BootRefusal>(|e| matches!(e, BootRefusal::ManifestMissing { .. }))
        .lacks("without a host service");
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
        .variant::<LoadRefusal>(
            |e| matches!(e, LoadRefusal::UnknownEventKind { kind, .. } if kind == "acme-status"),
        );
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::ManifestMissing { component }
                if component.ends_with("orphan.wasm"))
        })
        // Operator wording pin.
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::ManifestNotFound { manifest, .. }
                if manifest.ends_with("modle.toml"))
        });
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::MissingCapabilities))
        })
        // Operator wording pin.
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
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::Manifest(ParseError::MissingCapabilities))
        })
        // Operator wording pin.
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
        .variant::<BootRefusal>(|e| matches!(e, BootRefusal::Manifest(ParseError::BlankModuleName)))
        .lacks("claimed twice")
        .lacks("read component")
        .lacks("compile");

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
        .variant::<CapabilityError>(|e| {
            matches!(e, CapabilityError::Undeclared(v)
                if v.capability == "logging" && v.wit_import.starts_with("nexum:host/logging"))
        });
}
