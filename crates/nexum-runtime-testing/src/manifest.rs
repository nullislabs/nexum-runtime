//! `component.toml` builder for tests.

use std::path::{Path, PathBuf};

/// How a test supplies the manifest: the three shapes the loader must
/// handle, including the absent one.
#[derive(Debug, Clone, derive_more::From)]
pub enum ManifestInput {
    /// No explicit path; the loader falls back to discovery beside the component.
    Beside,
    /// A path handed to the loader as-is, existing or not.
    #[from]
    Path(PathBuf),
    /// Manifest text written out at boot.
    #[from]
    Toml(String),
}

impl ManifestInput {
    /// Materialize inline text at `path`; [`Beside`](Self::Beside) resolves to nothing.
    pub fn resolve(&self, path: &Path) -> Option<PathBuf> {
        match self {
            Self::Beside => None,
            Self::Path(explicit) => Some(explicit.clone()),
            Self::Toml(toml) => {
                std::fs::write(path, toml).expect("write the test manifest");
                Some(path.to_path_buf())
            }
        }
    }
}

impl From<TestManifest> for ManifestInput {
    fn from(manifest: TestManifest) -> Self {
        Self::Toml(manifest.to_toml())
    }
}

/// Sugar over [`TestManifest::new`] for `manifest(name).require([..])`.
pub fn manifest(name: impl Into<String>) -> TestManifest {
    TestManifest::new(name)
}

/// Builder for positive-path manifest TOML.
#[derive(Debug, Clone)]
pub struct TestManifest {
    name: String,
    component: Option<String>,
    caps: Vec<String>,
    http_allow: Vec<String>,
    config: Vec<(String, String)>,
    triggers: Vec<toml::Table>,
}

impl TestManifest {
    /// A minimal valid manifest: a name and nothing else.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            component: None,
            caps: Vec::new(),
            http_allow: Vec::new(),
            config: Vec::new(),
            triggers: Vec::new(),
        }
    }

    /// Set `[component].digest` to a content-digest pin.
    #[must_use]
    pub fn component_digest(mut self, digest: impl Into<String>) -> Self {
        self.component = Some(digest.into());
        self
    }

    /// Append a `[dependencies]` key; the table is emitted even when empty.
    #[must_use]
    pub fn cap(mut self, cap: impl Into<String>) -> Self {
        self.caps.push(cap.into());
        self
    }

    /// Several `[dependencies]` keys at once; each lands as one [`cap`](Self::cap).
    #[must_use]
    pub fn require(mut self, caps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.caps.extend(caps.into_iter().map(Into::into));
        self
    }

    /// Append a host to the `http` dependency; implies that dependency.
    #[must_use]
    pub fn http_allow(mut self, host: impl Into<String>) -> Self {
        self.http_allow.push(host.into());
        self
    }

    /// Add a `[[trigger]]` on new blocks for one chain.
    #[must_use]
    pub fn block_trigger(mut self, chain_id: u64) -> Self {
        self.triggers.push(trigger("block", chain_id));
        self
    }

    /// Append an unfiltered `event` trigger on `chain_id`.
    #[must_use]
    pub fn event_trigger(mut self, chain_id: u64) -> Self {
        self.triggers.push(trigger("event", chain_id));
        self
    }

    /// Append a filtered `event` trigger; an omitted filter key is absent
    /// from the emitted table, never empty.
    #[must_use]
    pub fn event_trigger_filtered(
        mut self,
        chain_id: u64,
        address: Option<&str>,
        event_signature: Option<&str>,
    ) -> Self {
        let mut table = trigger("event", chain_id);
        if let Some(address) = address {
            table.insert("address".into(), address.into());
        }
        if let Some(signature) = event_signature {
            table.insert("event_signature".into(), signature.into());
        }
        self.triggers.push(table);
        self
    }

    /// Append an extension trigger; no filters admits every delivery of the kind.
    #[must_use]
    pub fn extension_trigger(mut self, kind: &str, filters: &[(&str, &str)]) -> Self {
        let mut table = toml::Table::new();
        table.insert("on".into(), kind.into());
        for (key, value) in filters {
            table.insert((*key).into(), (*value).into());
        }
        self.triggers.push(table);
        self
    }

    /// Append a `[config]` key; values are TOML strings.
    #[must_use]
    pub fn config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.push((key.into(), value.into()));
        self
    }

    /// Render the manifest text. Built through `toml::Table` rather than
    /// string formatting, so a test cannot assert against TOML the real
    /// parser would reject.
    pub fn to_toml(&self) -> String {
        let mut component = toml::Table::new();
        component.insert("name".into(), self.name.clone().into());
        if let Some(digest) = &self.component {
            component.insert("digest".into(), digest.clone().into());
        }

        // Each dependency is a table, so an attribute belongs to the thing
        // it qualifies. `hosts` implies the http dependency.
        let mut dependencies = toml::Table::new();
        for cap in &self.caps {
            dependencies.insert(cap.clone(), toml::Table::new().into());
        }
        if !self.http_allow.is_empty() {
            let hosts: Vec<toml::Value> =
                self.http_allow.iter().map(|h| h.clone().into()).collect();
            let mut http = toml::Table::new();
            http.insert("hosts".into(), hosts.into());
            dependencies.insert("http".into(), http.into());
        }

        let mut root = toml::Table::new();
        root.insert("component".into(), component.into());
        root.insert("dependencies".into(), dependencies.into());
        if !self.config.is_empty() {
            let config: toml::Table = self
                .config
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect();
            root.insert("config".into(), config.into());
        }
        if !self.triggers.is_empty() {
            let triggers: Vec<toml::Value> =
                self.triggers.iter().map(|s| s.clone().into()).collect();
            root.insert("trigger".into(), triggers.into());
        }
        toml::to_string(&root).expect("serialize the test manifest")
    }

    /// Write the manifest as `component.toml` under `dir` and return its path.
    pub fn write_to(&self, dir: &Path) -> PathBuf {
        self.write_as(&dir.join("component.toml"))
    }

    /// Write to an exact path, for a test that hands the loader a name
    /// other than `component.toml`.
    pub fn write_as(&self, path: &Path) -> PathBuf {
        std::fs::write(path, self.to_toml()).expect("write the test manifest");
        path.to_path_buf()
    }
}

fn trigger(on: &str, chain_id: u64) -> toml::Table {
    let mut table = toml::Table::new();
    table.insert("on".into(), on.into());
    let chain_id = i64::try_from(chain_id).expect("chain id fits a TOML integer");
    table.insert("chain_id".into(), chain_id.into());
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexum_runtime_manifest::{CapabilityRegistry, LoadedManifest, Trigger, load};

    /// Load through the real write-then-parse path with the core registry.
    fn load_core(manifest: &TestManifest) -> LoadedManifest {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = manifest.write_to(dir.path());
        load(&path, &CapabilityRegistry::core()).expect("emitted manifest loads")
    }

    fn load_path(path: &Path) -> LoadedManifest {
        load(path, &CapabilityRegistry::core()).expect("emitted manifest loads")
    }

    #[test]
    fn emitted_manifest_loads_with_name_caps_and_triggers() {
        let loaded = load_core(
            &TestManifest::new("example")
                .cap("logging")
                .cap("chain")
                .block_trigger(1)
                .event_trigger(11_155_111),
        );

        assert_eq!(loaded.name.as_str(), "example");
        assert_eq!(
            loaded
                .dependencies
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["chain", "logging"],
        );

        let triggers = &loaded.triggers;
        assert_eq!(triggers.len(), 2, "both triggers parsed: {triggers:?}");
        assert!(matches!(triggers[0], Trigger::Block { chain_id: 1 }));
        assert!(matches!(
            triggers[1],
            Trigger::Event {
                chain_id: 11_155_111,
                address: None,
                event_signature: None,
                resume: false,
                max_lookback: None,
            }
        ));
    }

    #[test]
    fn digest_and_config_reach_the_loaded_manifest() {
        const DIGEST: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let loaded = load_core(
            &TestManifest::new("price-provider")
                .component_digest(DIGEST)
                .cap("logging")
                .config("threshold", "2500.00")
                .config("quoted", "a \"quoted\" value"),
        );

        assert_eq!(loaded.name.as_str(), "price-provider");
        assert_eq!(
            loaded.component_digest.expect("digest parsed").to_string(),
            DIGEST,
        );
        assert_eq!(
            loaded.config,
            vec![
                ("quoted".to_owned(), "a \"quoted\" value".to_owned()),
                ("threshold".to_owned(), "2500.00".to_owned()),
            ],
            "config pairs survive TOML escaping; the loader yields them in key order",
        );
    }

    #[test]
    fn dependency_table_is_emitted_even_without_entries() {
        let loaded = load_core(&TestManifest::new("bare"));
        assert!(loaded.dependencies.is_empty());
    }

    #[test]
    fn http_allow_reaches_the_loaded_allowlist_beside_the_required_caps() {
        let loaded = load_core(
            &TestManifest::new("probe")
                .cap("logging")
                .cap("http")
                .http_allow("127.0.0.1")
                .http_allow("*.acme.example"),
        );

        assert_eq!(
            loaded.http_allowlist,
            [
                nexum_primitives::host_pattern::HostPattern::from("127.0.0.1"),
                nexum_primitives::host_pattern::HostPattern::from("*.acme.example"),
            ]
        );
        assert_eq!(
            loaded
                .dependencies
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["http", "logging"],
            "the http attributes must not displace a sibling dependency",
        );
    }

    #[test]
    fn event_filters_and_extension_kinds_reach_the_loaded_triggers() {
        const ADDRESS: &str = "0xbA3cB449bD2B4ADddBc894D8697F5170800EAdeC";
        const TOPIC: &str = "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9";
        let address: alloy_primitives::Address = ADDRESS.parse().unwrap();
        let topic: alloy_primitives::B256 = TOPIC.parse().unwrap();

        let loaded = load_core(
            &TestManifest::new("example")
                .cap("logging")
                .event_trigger_filtered(1, Some(ADDRESS), Some(TOPIC))
                .event_trigger_filtered(2, Some(ADDRESS), None)
                .extension_trigger("acme-status", &[])
                .extension_trigger("acme-status", &[("scope", "primary")]),
        );

        let triggers = &loaded.triggers;
        assert!(
            matches!(
                &triggers[0],
                Trigger::Event { chain_id: 1, address: Some(a), event_signature: Some(t), .. }
                    if *a == address && *t == topic
            ),
            "both filters land: {triggers:?}",
        );
        assert!(
            matches!(
                &triggers[1],
                Trigger::Event { chain_id: 2, address: Some(a), event_signature: None, .. }
                    if *a == address
            ),
            "an omitted topic stays unfiltered: {triggers:?}",
        );
        assert!(
            matches!(&triggers[2], Trigger::Extension { extension_kind, filters }
                if extension_kind == "acme-status" && filters.is_empty()),
            "an unknown kind parses as an extension trigger: {triggers:?}",
        );
        assert!(
            matches!(&triggers[3], Trigger::Extension { extension_kind, filters }
                if extension_kind == "acme-status"
                    && filters.get("scope").is_some_and(|v| v == "primary")),
            "attribute filters ride the same table: {triggers:?}",
        );
    }

    /// The fluent entry point is pure sugar: `manifest(name).require([..])`
    /// emits byte-identical TOML to the explicit `new` and `cap` chain.
    #[test]
    fn manifest_and_require_are_sugar_over_new_and_cap() {
        let sugar = manifest("example")
            .require(["logging", "chain"])
            .block_trigger(1)
            .event_trigger(100)
            .to_toml();
        let explicit = TestManifest::new("example")
            .cap("logging")
            .cap("chain")
            .block_trigger(1)
            .event_trigger(100)
            .to_toml();
        assert_eq!(sugar, explicit);
    }

    /// Pins the emitted text itself, so a change to the serialized shape
    /// shows up in review rather than hiding behind loader compatibility.
    #[test]
    fn to_toml_emits_the_exact_golden_text() {
        let toml = TestManifest::new("golden")
            .cap("logging")
            .cap("http")
            .http_allow("127.0.0.1")
            .config("threshold", "2500.00")
            .block_trigger(1)
            .event_trigger_filtered(11_155_111, Some("0xabc"), None)
            .to_toml();
        let golden = r#"[component]
name = "golden"

[config]
threshold = "2500.00"

[dependencies.http]
hosts = ["127.0.0.1"]

[dependencies.logging]

[[trigger]]
chain_id = 1
on = "block"

[[trigger]]
address = "0xabc"
chain_id = 11155111
on = "event"
"#;
        assert_eq!(toml, golden);
    }

    #[test]
    fn manifest_sources_resolve_to_what_the_loader_receives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let at = dir.path().join("component.toml");

        assert_eq!(ManifestInput::Beside.resolve(&at), None);
        assert!(!at.exists(), "a discovered manifest writes nothing");

        let explicit = dir.path().join("absent.toml");
        assert_eq!(
            ManifestInput::from(explicit.clone()).resolve(&at),
            Some(explicit),
            "an explicit path passes through untouched",
        );
        assert!(!at.exists(), "an explicit path writes nothing");

        let inline = ManifestInput::from(TestManifest::new("inline").cap("logging"));
        assert_eq!(inline.resolve(&at).as_deref(), Some(at.as_path()));
        assert_eq!(load_path(&at).name.as_str(), "inline");
    }

    #[test]
    fn write_as_keeps_sibling_manifests_distinct() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = TestManifest::new("module-a")
            .cap("logging")
            .block_trigger(1)
            .write_as(&dir.path().join("a.toml"));
        let b = TestManifest::new("module-b")
            .cap("logging")
            .block_trigger(100)
            .write_as(&dir.path().join("b.toml"));

        assert_eq!(load_path(&a).name.as_str(), "module-a");
        assert_eq!(load_path(&b).name.as_str(), "module-b");
    }
}
