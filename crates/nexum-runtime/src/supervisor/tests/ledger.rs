//! Namespace and extension claims: the uniqueness ledger across roles.

use super::*;

/// A manifest section a wired extension claims passes; an unclaimed one
/// (a typo, or a section for an unwired extension) is refused.
#[test]
fn extension_sections_must_be_claimed() {
    struct Claiming;
    impl Extension<CoreRuntime> for Claiming {
        fn namespace(&self) -> &'static str {
            "acme"
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<CoreRuntime>>) -> anyhow::Result<()> {
            Ok(())
        }
        fn manifest_sections(&self) -> &'static [&'static str] {
            &["venue"]
        }
    }
    let extensions: Vec<Arc<dyn Extension<CoreRuntime>>> = vec![Arc::new(Claiming)];

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

/// Two extensions colliding on a subscription kind or a manifest section
/// are refused at boot; a non-colliding set passes the uniqueness pass.
#[test]
fn extension_claims_must_be_unique() {
    struct Claiming {
        namespace: &'static str,
        subscriptions: &'static [&'static str],
        sections: &'static [&'static str],
    }
    impl Extension<CoreRuntime> for Claiming {
        fn namespace(&self) -> &'static str {
            self.namespace
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<CoreRuntime>>) -> anyhow::Result<()> {
            Ok(())
        }
        fn subscriptions(&self) -> &'static [&'static str] {
            self.subscriptions
        }
        fn manifest_sections(&self) -> &'static [&'static str] {
            self.sections
        }
    }
    fn ext(
        namespace: &'static str,
        subscriptions: &'static [&'static str],
        sections: &'static [&'static str],
    ) -> Arc<dyn Extension<CoreRuntime>> {
        Arc::new(Claiming {
            namespace,
            subscriptions,
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
    .expect_err("duplicate subscription kind");
    assert!(
        matches!(&err, LoadRefusal::SubscriptionKindClaimed { kind } if *kind == "orders"),
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
fn claim_namespace_rejects_cross_role_duplicate_with_both_paths() {
    let mut ledger = NamespaceLedger::new();
    claim_namespace(
        &mut ledger,
        "price-alert",
        "module",
        Path::new("modules/price-alert.wasm"),
    )
    .expect("first claim");
    let err = claim_namespace(
        &mut ledger,
        "price-alert",
        "service",
        Path::new("services/impostor.wasm"),
    )
    .expect_err("cross-role duplicate must be refused");
    assert!(
        matches!(&err, BootRefusal::NamespaceClaimed { name, held_role: "module", held, role: "service", path }
            if name == "price-alert"
                && held.as_path() == Path::new("modules/price-alert.wasm")
                && path.as_path() == Path::new("services/impostor.wasm")),
        "the refusal names both claimants: {err}",
    );
}

#[test]
fn claim_namespace_is_byte_exact() {
    let mut ledger = NamespaceLedger::new();
    claim_namespace(&mut ledger, "Price-Alert", "module", Path::new("a.wasm"))
        .expect("case variant is a distinct name");
    claim_namespace(&mut ledger, "price-alert", "module", Path::new("b.wasm"))
        .expect("case variant is a distinct name");
    claim_namespace(&mut ledger, "price-alert", "module", Path::new("c.wasm"))
        .expect_err("identical strings collide");
}

/// One ledger spans both roles: a cross-role collision names both claimants,
/// a module-module duplicate hits the same gate, and neither reaches a compile.
#[tokio::test]
async fn boot_rejects_duplicate_names_across_and_within_roles() {
    let scenario = BootScenario::over(mock_components()).extensions(acme_extensions());
    let (adapter_wasm, module_wasm) = (
        scenario.dir().join("missing-adapter.wasm"),
        scenario.dir().join("missing-module.wasm"),
    );
    scenario
        .adapter(
            // Named to collide with the module below: the namespace claim
            // runs before the kind is resolved, so the collision is what fires.
            Entry::new(TestManifest::new("dup").kind("service").cap("chain")).wasm(adapter_wasm),
        )
        .module(Entry::new(TestManifest::new("dup").cap("logging")).wasm(module_wasm))
        .expect_refusal()
        .await
        .variant::<BootRefusal>(|e| {
            matches!(e, BootRefusal::NamespaceClaimed { name, held_role: "service", held, role: "module", path }
                if name == "dup"
                    && held.ends_with("missing-adapter.wasm")
                    && path.ends_with("missing-module.wasm"))
        })
        .lacks("compile");

    let scenario = BootScenario::new();
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
            matches!(e, BootRefusal::NamespaceClaimed { name, held_role: "module", held, role: "module", path }
                if name == "dup"
                    && held.ends_with("missing-a.wasm")
                    && path.ends_with("missing-b.wasm"))
        })
        .lacks("compile");
}
