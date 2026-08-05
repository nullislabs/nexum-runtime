//! `module.toml` builder for tests; the emitted text feeds the real
//! write-file-then-load path.

use std::path::{Path, PathBuf};

/// Builds positive-path manifest TOML. Negative-grammar fixtures stay raw
/// TOML at their test sites, where the textual malformation is the test.
#[derive(Debug, Clone)]
pub struct TestManifest {
    name: String,
    kind: Option<String>,
    component: Option<String>,
    caps: Vec<String>,
    http_allow: Vec<String>,
    config: Vec<(String, String)>,
    subscriptions: Vec<toml::Table>,
}

impl TestManifest {
    /// A manifest for the module `name` with no capabilities, config, or
    /// subscriptions.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: None,
            component: None,
            caps: Vec::new(),
            http_allow: Vec::new(),
            config: Vec::new(),
            subscriptions: Vec::new(),
        }
    }

    /// Set `[module].kind`; unset defaults to the worker at load.
    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Set `[module].component` to a content-digest pin.
    pub fn component_digest(mut self, digest: impl Into<String>) -> Self {
        self.component = Some(digest.into());
        self
    }

    /// Append one entry to `[capabilities].required`; the section is always
    /// emitted, so an entry-less build still passes the capsless gate.
    pub fn cap(mut self, cap: impl Into<String>) -> Self {
        self.caps.push(cap.into());
        self
    }

    /// Append one host to `[capabilities.http].allow`; the section is
    /// emitted only once a host is added, so the default stays allowlist-free.
    pub fn http_allow(mut self, host: impl Into<String>) -> Self {
        self.http_allow.push(host.into());
        self
    }

    /// Append a `block` subscription on `chain_id`.
    pub fn block_sub(mut self, chain_id: u64) -> Self {
        self.subscriptions.push(subscription("block", chain_id));
        self
    }

    /// Append a `chain-log` subscription on `chain_id` with no address or
    /// topic filter, so any pushed log matches.
    pub fn chain_log_sub(mut self, chain_id: u64) -> Self {
        self.subscriptions.push(subscription("chain-log", chain_id));
        self
    }

    /// Append a `chain-log` subscription narrowed by address, topic-0, or
    /// both; an omitted filter key is absent from the emitted table rather
    /// than empty, which is what the loader reads as unfiltered.
    pub fn chain_log_sub_filtered(
        mut self,
        chain_id: u64,
        address: Option<&str>,
        event_signature: Option<&str>,
    ) -> Self {
        let mut sub = subscription("chain-log", chain_id);
        if let Some(address) = address {
            sub.insert("address".into(), address.into());
        }
        if let Some(signature) = event_signature {
            sub.insert("event_signature".into(), signature.into());
        }
        self.subscriptions.push(sub);
        self
    }

    /// Append an extension-owned subscription `kind` with attribute
    /// `filters`; no filter admits every event of the kind.
    pub fn extension_sub(mut self, kind: &str, filters: &[(&str, &str)]) -> Self {
        let mut sub = toml::Table::new();
        sub.insert("kind".into(), kind.into());
        for (key, value) in filters {
            sub.insert((*key).into(), (*value).into());
        }
        self.subscriptions.push(sub);
        self
    }

    /// Append one `[config]` key; values are TOML strings, matching what a
    /// module's `init` receives.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.push((key.into(), value.into()));
        self
    }

    /// Render the manifest as TOML text.
    pub fn to_toml(&self) -> String {
        let mut module = toml::Table::new();
        module.insert("name".into(), self.name.clone().into());
        if let Some(kind) = &self.kind {
            module.insert("kind".into(), kind.clone().into());
        }
        if let Some(component) = &self.component {
            module.insert("component".into(), component.clone().into());
        }

        let mut capabilities = toml::Table::new();
        let required: Vec<toml::Value> = self.caps.iter().map(|c| c.clone().into()).collect();
        capabilities.insert("required".into(), required.into());
        if !self.http_allow.is_empty() {
            let allow: Vec<toml::Value> =
                self.http_allow.iter().map(|h| h.clone().into()).collect();
            let mut http = toml::Table::new();
            http.insert("allow".into(), allow.into());
            capabilities.insert("http".into(), http.into());
        }

        let mut root = toml::Table::new();
        root.insert("module".into(), module.into());
        root.insert("capabilities".into(), capabilities.into());
        if !self.config.is_empty() {
            let config: toml::Table = self
                .config
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect();
            root.insert("config".into(), config.into());
        }
        if !self.subscriptions.is_empty() {
            let subs: Vec<toml::Value> = self
                .subscriptions
                .iter()
                .map(|s| s.clone().into())
                .collect();
            root.insert("subscription".into(), subs.into());
        }
        toml::to_string(&root).expect("serialise the test manifest")
    }

    /// Write the manifest as `module.toml` under `dir` and return its path.
    pub fn write_to(&self, dir: &Path) -> PathBuf {
        self.write_as(&dir.join("module.toml"))
    }

    /// Write the manifest to `path` and return it; a multi-module test needs
    /// distinct names in one directory, which `module.toml` cannot give.
    pub fn write_as(&self, path: &Path) -> PathBuf {
        std::fs::write(path, self.to_toml()).expect("write the test manifest");
        path.to_path_buf()
    }
}

/// One `[[subscription]]` table of the core `kind` on `chain_id`.
fn subscription(kind: &str, chain_id: u64) -> toml::Table {
    let mut sub = toml::Table::new();
    sub.insert("kind".into(), kind.into());
    let chain_id = i64::try_from(chain_id).expect("chain id fits a TOML integer");
    sub.insert("chain_id".into(), chain_id.into());
    sub
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CapabilityRegistry, ComponentKind, Subscription, load};

    /// Load `manifest` through the real file-then-parse path with the core
    /// capability registry.
    fn load_core(manifest: &TestManifest) -> crate::manifest::LoadedManifest {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = manifest.write_to(dir.path());
        load(&path, &CapabilityRegistry::core()).expect("emitted manifest loads")
    }

    /// Load `path` with the core capability registry.
    fn load_path(path: &Path) -> crate::manifest::LoadedManifest {
        load(path, &CapabilityRegistry::core()).expect("emitted manifest loads")
    }

    #[test]
    fn emitted_manifest_loads_with_name_caps_and_subscriptions() {
        let loaded = load_core(
            &TestManifest::new("example")
                .cap("logging")
                .cap("chain")
                .block_sub(1)
                .chain_log_sub(11_155_111),
        );

        assert_eq!(loaded.manifest.module.name, "example");
        assert_eq!(loaded.manifest.module.kind, ComponentKind::Worker);
        let caps = loaded.manifest.capabilities.expect("capabilities section");
        assert_eq!(caps.required, ["logging", "chain"]);

        let subs = &loaded.manifest.subscriptions;
        assert_eq!(subs.len(), 2, "both subscriptions parsed: {subs:?}");
        assert!(matches!(subs[0], Subscription::Block { chain_id: 1 }));
        assert!(matches!(
            subs[1],
            Subscription::ChainLog {
                chain_id: 11_155_111,
                address: None,
                event_signature: None,
                resume: false,
                max_lookback: None,
            }
        ));
    }

    #[test]
    fn kind_component_and_config_reach_the_loaded_manifest() {
        let loaded = load_core(
            &TestManifest::new("feeder")
                .kind("price-provider")
                .component_digest("sha256:abc123")
                .cap("logging")
                .config("threshold", "2500.00")
                .config("quoted", "a \"quoted\" value"),
        );

        assert_eq!(
            loaded.manifest.module.kind,
            ComponentKind::Provider("price-provider".into()),
        );
        assert_eq!(loaded.manifest.module.component, "sha256:abc123");
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
    fn capabilities_section_is_emitted_even_without_entries() {
        let loaded = load_core(&TestManifest::new("bare"));
        let caps = loaded.manifest.capabilities.expect("capabilities section");
        assert!(caps.required.is_empty());
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

        assert_eq!(loaded.http_allowlist, ["127.0.0.1", "*.acme.example"]);
        let caps = loaded.manifest.capabilities.expect("capabilities section");
        assert_eq!(
            caps.required,
            ["logging", "http"],
            "the nested http table must not swallow the sibling required key",
        );
    }

    #[test]
    fn chain_log_filters_and_extension_kinds_reach_the_loaded_subscriptions() {
        const ADDRESS: &str = "0xbA3cB449bD2B4ADddBc894D8697F5170800EAdeC";
        const TOPIC: &str = "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9";

        let loaded = load_core(
            &TestManifest::new("example")
                .cap("logging")
                .chain_log_sub_filtered(1, Some(ADDRESS), Some(TOPIC))
                .chain_log_sub_filtered(2, Some(ADDRESS), None)
                .extension_sub("acme-status", &[])
                .extension_sub("acme-status", &[("scope", "primary")]),
        );

        let subs = &loaded.manifest.subscriptions;
        assert!(
            matches!(
                &subs[0],
                Subscription::ChainLog { chain_id: 1, address: Some(a), event_signature: Some(t), .. }
                    if a == ADDRESS && t == TOPIC
            ),
            "both filters land: {subs:?}",
        );
        assert!(
            matches!(
                &subs[1],
                Subscription::ChainLog { chain_id: 2, address: Some(a), event_signature: None, .. }
                    if a == ADDRESS
            ),
            "an omitted topic stays unfiltered: {subs:?}",
        );
        assert!(
            matches!(&subs[2], Subscription::Extension { kind, filters }
                if kind == "acme-status" && filters.is_empty()),
            "an unknown kind parses as an extension subscription: {subs:?}",
        );
        assert!(
            matches!(&subs[3], Subscription::Extension { kind, filters }
                if kind == "acme-status" && filters.get("scope").is_some_and(|v| v == "primary")),
            "attribute filters ride the same table: {subs:?}",
        );
    }

    #[test]
    fn write_as_keeps_sibling_manifests_distinct() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = TestManifest::new("module-a")
            .cap("logging")
            .block_sub(1)
            .write_as(&dir.path().join("a.toml"));
        let b = TestManifest::new("module-b")
            .cap("logging")
            .block_sub(100)
            .write_as(&dir.path().join("b.toml"));

        assert_eq!(load_path(&a).manifest.module.name, "module-a");
        assert_eq!(load_path(&b).manifest.module.name, "module-b");
    }
}
