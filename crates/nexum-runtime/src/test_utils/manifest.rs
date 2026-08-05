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
        let path = dir.join("module.toml");
        std::fs::write(&path, self.to_toml()).expect("write the test manifest");
        path
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
}
