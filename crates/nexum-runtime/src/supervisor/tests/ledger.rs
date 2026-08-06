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

#[tokio::test]
async fn boot_rejects_duplicate_module_names_before_any_compile() {
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let tmp = tempfile::tempdir().unwrap();
    let manifest_a = tmp.path().join("a.toml");
    let manifest_b = tmp.path().join("b.toml");
    let manifest_toml = "[module]\nname = \"dup\"\n\n[capabilities]\nrequired = [\"logging\"]\n";
    std::fs::write(&manifest_a, manifest_toml).unwrap();
    std::fs::write(&manifest_b, manifest_toml).unwrap();

    let engine_cfg = EngineConfig {
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: tmp.path().join("missing-a.wasm"),
                manifest: Some(manifest_a),
            },
            crate::engine_config::ModuleEntry {
                path: tmp.path().join("missing-b.wasm"),
                manifest: Some(manifest_b),
            },
        ],
        ..Default::default()
    };

    let err = match Supervisor::boot(
        &engine,
        &linker,
        &engine_cfg,
        &components,
        &core_extensions(),
        None,
    )
    .await
    {
        Ok(_) => panic!("duplicate module names must refuse the boot"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("name dup is claimed twice"),
        "the refusal is the claim collision: {msg}",
    );
    assert!(
        msg.contains("missing-a.wasm") && msg.contains("missing-b.wasm"),
        "the refusal names both claimant paths: {msg}",
    );
    assert!(
        !msg.contains("compile"),
        "rejection precedes any compile of the missing wasm: {msg}",
    );
}

/// One ledger spans both roles.
#[tokio::test]
async fn boot_rejects_a_module_colliding_with_an_adapter_name() {
    let engine = make_wasmtime_engine();
    let components = crate::test_utils::mock_components();
    let extensions = acme_extensions();
    let linker =
        crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &extensions)
            .expect("build_linker");

    let dir = tempfile::tempdir().expect("tempdir");
    let adapter_manifest = dir.path().join("adapter.toml");
    std::fs::write(
        &adapter_manifest,
        "[module]\nname = \"dup\"\nkind = \"acme-adapter\"\n\n\
         [capabilities]\nrequired = [\"chain\"]\n",
    )
    .expect("write adapter manifest");
    let module_manifest = dir.path().join("module.toml");
    std::fs::write(
        &module_manifest,
        "[module]\nname = \"dup\"\n\n[capabilities]\nrequired = [\"logging\"]\n",
    )
    .expect("write module manifest");

    let config = EngineConfig {
        adapters: vec![crate::engine_config::AdapterEntry {
            path: dir.path().join("missing-adapter.wasm"),
            manifest: Some(adapter_manifest),
            http_allow: Vec::new(),
            messaging_topics: Vec::new(),
        }],
        modules: vec![crate::engine_config::ModuleEntry {
            path: dir.path().join("missing-module.wasm"),
            manifest: Some(module_manifest),
        }],
        ..Default::default()
    };

    let err =
        match Supervisor::boot(&engine, &linker, &config, &components, &extensions, None).await {
            Ok(_) => panic!("a module colliding with an adapter name must refuse the boot"),
            Err(err) => err,
        };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("name dup is claimed twice"),
        "the refusal is the claim collision: {msg}",
    );
    assert!(
        msg.contains("adapter") && msg.contains("module"),
        "the refusal names both roles: {msg}",
    );
    assert!(
        msg.contains("missing-adapter.wasm") && msg.contains("missing-module.wasm"),
        "the refusal names both claimant paths: {msg}",
    );
    assert!(
        !msg.contains("compile"),
        "one ledger spans both roles, so no compile precedes the refusal: {msg}",
    );
}
