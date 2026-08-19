//! Namespace and extension claims: the uniqueness ledger.

use super::*;

/// A manifest section a wired extension claims passes; an unclaimed one
/// (a typo, or a section for an unwired extension) is refused.
#[test]
fn extension_sections_must_be_claimed() {
    struct Claiming;
    impl Extension<LocalTypes> for Claiming {
        fn namespace(&self) -> &'static str {
            "acme"
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(
            &self,
            _linker: &mut Linker<HostState<LocalTypes>>,
        ) -> Result<(), nexum_runtime_api::ExtensionError> {
            Ok(())
        }
        fn manifest_sections(&self) -> &'static [&'static str] {
            &["venue"]
        }
    }
    let extensions: Vec<Arc<dyn Extension<LocalTypes>>> = vec![Arc::new(Claiming)];

    let mut sections = manifest::ExtensionSections::new();
    sections.insert("venue".into(), toml::Value::Boolean(true));
    enforce_extension_sections("keeper", &sections, &extensions).expect("claimed section");

    sections.insert("venu".into(), toml::Value::Boolean(true));
    let err = enforce_extension_sections("keeper", &sections, &extensions)
        .expect_err("unclaimed section");
    assert!(
        matches!(&err, LoadRefusal::SectionUnclaimed { owner, section }
            if owner == "keeper" && section == "venu"),
        "{err}"
    );
}

/// Two extensions colliding on a trigger kind or a manifest section
/// are refused at boot; a non-colliding set passes the uniqueness pass.
#[test]
fn extension_claims_must_be_unique() {
    struct Claiming {
        namespace: &'static str,
        kinds: &'static [&'static str],
        sections: &'static [&'static str],
    }
    impl Extension<LocalTypes> for Claiming {
        fn namespace(&self) -> &'static str {
            self.namespace
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(
            &self,
            _linker: &mut Linker<HostState<LocalTypes>>,
        ) -> Result<(), nexum_runtime_api::ExtensionError> {
            Ok(())
        }
        fn emits_trigger_kinds(&self) -> &'static [&'static str] {
            self.kinds
        }
        fn manifest_sections(&self) -> &'static [&'static str] {
            self.sections
        }
    }
    fn ext(
        namespace: &'static str,
        kinds: &'static [&'static str],
        sections: &'static [&'static str],
    ) -> Arc<dyn Extension<LocalTypes>> {
        Arc::new(Claiming {
            namespace,
            kinds,
            sections,
        })
    }

    enforce_extension_uniqueness(&[
        ext("a", &["orders"], &["venue"]),
        ext("b", &["fills"], &["pool"]),
    ])
    .expect("non-colliding set boots");

    let err = enforce_extension_uniqueness(&[
        ext("a", &["orders"], &["venue"]),
        ext("b", &["orders"], &["pool"]),
    ])
    .expect_err("duplicate trigger kind");
    assert!(
        matches!(&err, LoadRefusal::TriggerKindClaimed { kind } if *kind == "orders"),
        "{err}"
    );

    let err = enforce_extension_uniqueness(&[
        ext("a", &["orders"], &["venue"]),
        ext("b", &["fills"], &["venue"]),
    ])
    .expect_err("duplicate manifest section");
    assert!(
        matches!(&err, LoadRefusal::SectionClaimed { section } if *section == "venue"),
        "{err}"
    );
}

#[test]
fn claim_namespace_rejects_a_duplicate_with_both_paths() {
    let mut ledger = NamespaceLedger::new();
    claim_namespace(
        &mut ledger,
        "price-alert",
        Path::new("modules/price-alert.wasm"),
    )
    .expect("first claim");
    let err = claim_namespace(
        &mut ledger,
        "price-alert",
        Path::new("modules/impostor.wasm"),
    )
    .expect_err("duplicate must be refused");
    assert!(
        matches!(&err, BootRefusal::NamespaceClaimed { name, held, path }
            if name == "price-alert"
                && held.as_path() == Path::new("modules/price-alert.wasm")
                && path.as_path() == Path::new("modules/impostor.wasm")),
        "the refusal names both claimants: {err}",
    );
}

#[test]
fn claim_namespace_is_byte_exact() {
    let mut ledger = NamespaceLedger::new();
    claim_namespace(&mut ledger, "Price-Alert", Path::new("a.wasm"))
        .expect("case variant is a distinct name");
    claim_namespace(&mut ledger, "price-alert", Path::new("b.wasm"))
        .expect("case variant is a distinct name");
    claim_namespace(&mut ledger, "price-alert", Path::new("c.wasm"))
        .expect_err("identical strings collide");
}

/// A module-module duplicate refuses before any compile and names both
/// claimants.
#[tokio::test]
async fn boot_rejects_a_duplicate_module_name() {
    let scenario = scenario();
    let (first, second) = (
        scenario.dir().join("missing-a.wasm"),
        scenario.dir().join("missing-b.wasm"),
    );
    scenario
        .module(Entry::new(TestManifest::new("dup").cap("logging")).wasm(first))
        .module(Entry::new(TestManifest::new("dup").cap("logging")).wasm(second))
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::NamespaceClaimed { name, held, path }
                if name == "dup"
                    && held.ends_with("missing-a.wasm")
                    && path.ends_with("missing-b.wasm"))
        })
        .lacks("compile");
}
