//! Namespace and extension claims: the uniqueness ledger across roles.

use super::*;

/// A manifest section a wired extension claims passes; an unclaimed one
/// (a typo, or a section for an unwired extension) is refused.
#[test]
fn extension_sections_must_be_claimed() {
    struct Claiming;
    impl Extension<TestTypes> for Claiming {
        fn namespace(&self) -> &'static str {
            "acme"
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<TestTypes>>) -> anyhow::Result<()> {
            Ok(())
        }
        fn manifest_sections(&self) -> &'static [&'static str] {
            &["venue"]
        }
    }
    let extensions: Vec<Arc<dyn Extension<TestTypes>>> = vec![Arc::new(Claiming)];

    let mut sections = manifest::ExtensionSections::new();
    sections.insert("venue".into(), toml::Value::Boolean(true));
    enforce_extension_sections("keeper", &sections, &extensions).expect("claimed section");

    sections.insert("venu".into(), toml::Value::Boolean(true));
    let err = enforce_extension_sections("keeper", &sections, &extensions)
        .expect_err("unclaimed section");
    assert!(err.to_string().contains("[venu]"), "{err}");
    assert!(err.to_string().contains("keeper"), "{err}");
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
    impl Extension<TestTypes> for Claiming {
        fn namespace(&self) -> &'static str {
            self.namespace
        }
        fn capabilities(&self) -> crate::manifest::NamespaceCaps {
            crate::manifest::NamespaceCaps {
                prefix: "acme:ext/",
                ifaces: &[],
            }
        }
        fn link(&self, _linker: &mut Linker<HostState<TestTypes>>) -> anyhow::Result<()> {
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
    ) -> Arc<dyn Extension<TestTypes>> {
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
    assert!(err.to_string().contains("orders"), "{err}");

    let err = enforce_extension_uniqueness(&[
        ext("a", &["orders"], &["venue"]),
        ext("b", &["fills"], &["venue"]),
    ])
    .expect_err("duplicate manifest section");
    assert!(err.to_string().contains("[venue]"), "{err}");
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
        "adapter",
        Path::new("adapters/impostor.wasm"),
    )
    .expect_err("cross-role duplicate must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("module") && msg.contains("adapter"),
        "the refusal names both roles: {msg}",
    );
    assert!(
        msg.contains("modules/price-alert.wasm") && msg.contains("adapters/impostor.wasm"),
        "the refusal names both claimant paths: {msg}",
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
            Entry::new(TestManifest::new("dup").kind("acme-adapter").cap("chain"))
                .wasm(adapter_wasm),
        )
        .module(Entry::new(TestManifest::new("dup").cap("logging")).wasm(module_wasm))
        .expect_refusal()
        .await
        .names("name dup is claimed twice")
        .names("adapter")
        .names("module")
        .names("missing-adapter.wasm")
        .names("missing-module.wasm")
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
        .names("name dup is claimed twice")
        .names("missing-a.wasm")
        .names("missing-b.wasm")
        .lacks("compile");
}
